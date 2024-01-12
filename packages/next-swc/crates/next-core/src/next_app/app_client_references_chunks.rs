use anyhow::Result;
use indexmap::IndexMap;
use tracing::Instrument;
use turbo_tasks::{TryFlatJoinIterExt, TryJoinIterExt, Value, ValueToString, Vc};
use turbopack_binding::turbopack::{
    core::{
        chunk::{availability_info::AvailabilityInfo, ChunkingContext, ChunkingContextExt},
        module::Module,
        output::OutputAssets,
    },
    ecmascript::chunk::EcmascriptChunkingContext,
};

use super::include_modules_module::IncludeModulesModule;
use crate::{
    next_client_reference::{ClientReferenceType, ClientReferenceTypes, ClientReferences},
    next_server_component::server_component_module::NextServerComponentModule,
};

#[turbo_tasks::function]
fn client_modules_modifier() -> Vc<String> {
    Vc::cell("client modules".to_string())
}

#[turbo_tasks::function]
fn client_modules_ssr_modifier() -> Vc<String> {
    Vc::cell("client modules ssr".to_string())
}

#[turbo_tasks::value]
pub struct ClientReferencesChunks {
    pub client_component_client_chunks: IndexMap<ClientReferenceType, Vc<OutputAssets>>,
    pub client_component_ssr_chunks: IndexMap<ClientReferenceType, Vc<OutputAssets>>,
    pub layout_segment_client_chunks: IndexMap<Vc<NextServerComponentModule>, Vc<OutputAssets>>,
}

/// Computes all client references chunks.
///
/// This returns a map from client reference type to the chunks that reference
/// type needs to load.
#[turbo_tasks::function]
pub async fn get_app_client_references_chunks(
    app_client_references: Vc<ClientReferences>,
    client_chunking_context: Vc<Box<dyn EcmascriptChunkingContext>>,
    ssr_chunking_context: Vc<Box<dyn EcmascriptChunkingContext>>,
) -> Result<Vc<ClientReferencesChunks>> {
    async move {
        // TODO Reconsider this. Maybe it need to be true in production.
        let separate_chunk_group_per_client_reference = false;
        let app_client_references = app_client_references.await?;
        if separate_chunk_group_per_client_reference {
            let app_client_references_chunks: Vec<(_, (_, Option<_>))> = app_client_references
                .iter()
                .map(|client_reference| async move {
                    let client_reference_ty = client_reference.ty();
                    Ok((
                        client_reference_ty,
                        match client_reference_ty {
                            ClientReferenceType::EcmascriptClientReference(
                                ecmascript_client_reference,
                            ) => {
                                let ecmascript_client_reference_ref =
                                    ecmascript_client_reference.await?;
                                (
                                    client_chunking_context.root_chunk_group_assets(Vc::upcast(
                                        ecmascript_client_reference_ref.client_module,
                                    )),
                                    Some(ssr_chunking_context.root_chunk_group_assets(Vc::upcast(
                                        ecmascript_client_reference_ref.ssr_module,
                                    ))),
                                )
                            }
                            ClientReferenceType::CssClientReference(css_client_reference) => {
                                let css_client_reference_ref = css_client_reference.await?;
                                (
                                    client_chunking_context.root_chunk_group_assets(Vc::upcast(
                                        css_client_reference_ref.client_module,
                                    )),
                                    None,
                                )
                            }
                        },
                    ))
                })
                .try_join()
                .await?;

            Ok(ClientReferencesChunks {
                client_component_client_chunks: app_client_references_chunks
                    .iter()
                    .map(|&(client_reference_ty, (client_chunks, _))| {
                        (client_reference_ty, client_chunks)
                    })
                    .collect(),
                client_component_ssr_chunks: app_client_references_chunks
                    .iter()
                    .flat_map(|&(client_reference_ty, (_, ssr_chunks))| {
                        ssr_chunks.map(|ssr_chunks| (client_reference_ty, ssr_chunks))
                    })
                    .collect(),
                layout_segment_client_chunks: IndexMap::new(),
            }
            .cell())
        } else {
            let mut client_references_by_server_component: IndexMap<_, Vec<_>> = IndexMap::new();
            let mut framework_reference_types = Vec::new();
            for client_reference in app_client_references {
                if let Some(server_component) = client_reference.server_component() {
                    client_references_by_server_component
                        .entry(server_component)
                        .or_default()
                        .push(client_reference.ty());
                } else {
                    framework_reference_types.push(client_reference.ty());
                }
            }
            // Framework components need to go into first layout segment
            if let Some((_, list)) = client_references_by_server_component.first_mut() {
                list.extend(framework_reference_types);
            }

            let mut current_client_availability_info = AvailabilityInfo::Root;
            let mut current_client_chunks = OutputAssets::empty();
            let mut current_ssr_availability_info = AvailabilityInfo::Root;
            let mut current_ssr_chunks = OutputAssets::empty();

            let mut layout_segment_client_chunks = IndexMap::new();
            let mut client_component_ssr_chunks = IndexMap::new();
            let mut client_component_client_chunks = IndexMap::new();

            for (server_component, client_reference_types) in
                client_references_by_server_component.into_iter()
            {
                let base_ident = server_component.ident();
                let ssr_modules = client_reference_types
                    .iter()
                    .map(|client_reference_ty| async move {
                        Ok(match client_reference_ty {
                            ClientReferenceType::EcmascriptClientReference(
                                ecmascript_client_reference,
                            ) => {
                                let ecmascript_client_reference_ref =
                                    ecmascript_client_reference.await?;
                                Some(Vc::upcast(ecmascript_client_reference_ref.ssr_module))
                            }
                            _ => None,
                        })
                    })
                    .try_flat_join()
                    .await?;
                let ssr_entry_module = IncludeModulesModule::new(
                    base_ident.with_modifier(client_modules_ssr_modifier()),
                    ssr_modules,
                );
                let client_modules = client_reference_types
                    .iter()
                    .map(|client_reference_ty| async move {
                        Ok(match client_reference_ty {
                            ClientReferenceType::EcmascriptClientReference(
                                ecmascript_client_reference,
                            ) => {
                                let ecmascript_client_reference_ref =
                                    ecmascript_client_reference.await?;
                                Vc::upcast(ecmascript_client_reference_ref.client_module)
                            }
                            ClientReferenceType::CssClientReference(css_client_reference) => {
                                let css_client_reference_ref = css_client_reference.await?;
                                Vc::upcast(css_client_reference_ref.client_module)
                            }
                        })
                    })
                    .try_join()
                    .await?;
                let client_entry_module = IncludeModulesModule::new(
                    base_ident.with_modifier(client_modules_modifier()),
                    client_modules,
                );
                let server_component_path = server_component.server_path().to_string().await?;
                let client_chunk_group = {
                    let _span = tracing::info_span!(
                        "client side rendering",
                        layout_segment = display(&server_component_path),
                    )
                    .entered();
                    client_chunking_context.chunk_group(
                        Vc::upcast(client_entry_module),
                        Value::new(current_client_availability_info),
                    )
                };
                let ssr_chunk_group = {
                    let _span = tracing::info_span!(
                        "server side rendering",
                        layout_segment = display(&server_component_path),
                    )
                    .entered();
                    ssr_chunking_context.chunk_group(
                        Vc::upcast(ssr_entry_module),
                        Value::new(current_ssr_availability_info),
                    )
                };
                let client_chunk_group = client_chunk_group.await?;
                let ssr_chunk_group = ssr_chunk_group.await?;

                current_client_availability_info = client_chunk_group.availability_info;
                current_ssr_availability_info = ssr_chunk_group.availability_info;

                current_client_chunks =
                    current_client_chunks.concatenate(client_chunk_group.assets);
                current_ssr_chunks = current_ssr_chunks.concatenate(ssr_chunk_group.assets);

                current_client_chunks = current_client_chunks.resolve().await?;
                current_ssr_chunks = current_ssr_chunks.resolve().await?;

                layout_segment_client_chunks.insert(server_component, current_client_chunks);

                for client_reference_ty in client_reference_types {
                    if let ClientReferenceType::EcmascriptClientReference(_) = client_reference_ty {
                        client_component_ssr_chunks.insert(client_reference_ty, current_ssr_chunks);
                        client_component_client_chunks
                            .insert(client_reference_ty, current_client_chunks);
                    }
                }
            }

            Ok(ClientReferencesChunks {
                client_component_client_chunks,
                client_component_ssr_chunks,
                layout_segment_client_chunks,
            }
            .cell())
        }
    }
    .instrument(tracing::info_span!("process client references"))
    .await
}

/// Crawls all modules emitted in the client transition, returning a list of all
/// client JS modules.
#[turbo_tasks::function]
pub async fn get_app_server_reference_modules(
    app_client_reference_types: Vc<ClientReferenceTypes>,
) -> Result<Vc<Vec<Vc<Box<dyn Module>>>>> {
    Ok(Vc::cell(
        app_client_reference_types
            .await?
            .iter()
            .map(|client_reference_ty| async move {
                Ok(match client_reference_ty {
                    ClientReferenceType::EcmascriptClientReference(ecmascript_client_reference) => {
                        let ecmascript_client_reference_ref = ecmascript_client_reference.await?;
                        Some(Vc::upcast(ecmascript_client_reference_ref.client_module))
                    }
                    _ => None,
                })
            })
            .try_flat_join()
            .await?,
    ))
}
