use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::{ByteView, SelfCell};
use symbolic::debuginfo::{Object, ObjectDebugSession};

use crate::services::objects::{FindObject, ObjectPurpose, ObjectsActor};
use crate::services::symcaches::{FetchSymCache, SymCacheActor, SymCacheFile};
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    CompleteObjectInfo, CompleteStacktrace, ObjectFileStatus, RawStacktrace, Scope,
};
use crate::utils::addr::AddrMode;

use super::object_id_from_object_info;

#[derive(Debug, Clone)]
pub struct SymCacheLookupResult<'a> {
    pub module_index: usize,
    pub object_info: &'a CompleteObjectInfo,
    pub symcache: Option<&'a SymCacheFile>,
    pub relative_addr: Option<u64>,
}

impl<'a> SymCacheLookupResult<'a> {
    /// The preferred [`AddrMode`] for this lookup.
    ///
    /// For the symbolicated frame, we generally switch to absolute reporting of addresses. This is
    /// not done for images mounted at `0` because, for instance, WASM does not have a unified
    /// address space and so it is not possible for us to absolutize addresses.
    pub fn preferred_addr_mode(&self) -> AddrMode {
        if self.object_info.supports_absolute_addresses() {
            AddrMode::Abs
        } else {
            AddrMode::Rel(self.module_index)
        }
    }

    /// Exposes an address consistent with [`preferred_addr_mode`](Self::preferred_addr_mode).
    pub fn expose_preferred_addr(&self, addr: u64) -> u64 {
        if self.object_info.supports_absolute_addresses() {
            self.object_info.rel_to_abs_addr(addr).unwrap_or(0)
        } else {
            addr
        }
    }
}

pub struct SourceObject(SelfCell<ByteView<'static>, Object<'static>>);

struct ModuleEntry {
    module_index: usize,
    object_info: CompleteObjectInfo,
    symcache: Option<Arc<SymCacheFile>>,
    source_object: Option<SourceObject>,
}

pub struct ModuleLookup {
    modules: Vec<ModuleEntry>,
    scope: Scope,
    sources: Arc<[SourceConfig]>,
}

impl ModuleLookup {
    /// Creates a new [`ModuleLookup`] out of the given module iterator.
    pub fn new<Iter>(scope: Scope, sources: Arc<[SourceConfig]>, iter: Iter) -> Self
    where
        Iter: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut modules: Vec<_> = iter
            .into_iter()
            .enumerate()
            .map(|(module_index, object_info)| ModuleEntry {
                module_index,
                object_info,
                symcache: None,
                source_object: None,
            })
            .collect();

        modules.sort_by_key(|entry| entry.object_info.raw.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        modules.dedup_by(|entry2, entry1| {
            // If this underflows we didn't sort properly.
            let size = entry2.object_info.raw.image_addr.0 - entry1.object_info.raw.image_addr.0;
            if entry1.object_info.raw.image_size.unwrap_or(0) == 0 {
                entry1.object_info.raw.image_size = Some(size);
            }

            false
        });

        Self {
            modules,
            scope,
            sources,
        }
    }

    /// Returns the original `CompleteObjectInfo` list in its original sorting order.
    pub fn into_inner(mut self) -> Vec<CompleteObjectInfo> {
        self.modules.sort_by_key(|entry| entry.module_index);
        self.modules
            .into_iter()
            .map(|entry| entry.object_info)
            .collect()
    }

    /// Fetches all the SymCaches for the modules referenced by the `stacktraces`.
    #[tracing::instrument(skip_all)]
    pub async fn fetch_symcaches(
        &mut self,
        symcache_actor: SymCacheActor,
        stacktraces: &[RawStacktrace],
    ) {
        let mut referenced_objects = HashSet::new();
        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
                if let Some(SymCacheLookupResult { module_index, .. }) =
                    self.lookup_symcache(frame.instruction_addr.0, frame.addr_mode)
                {
                    referenced_objects.insert(module_index);
                }
            }
        }

        let futures = self
            .modules
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let is_used = referenced_objects.contains(&entry.module_index);
                if !is_used {
                    entry.object_info.debug_status = ObjectFileStatus::Unused;
                    return None;
                }

                let symcache_actor = symcache_actor.clone();
                let request = FetchSymCache {
                    object_type: entry.object_info.raw.ty,
                    identifier: object_id_from_object_info(&entry.object_info.raw),
                    sources: self.sources.clone(),
                    scope: self.scope.clone(),
                };

                Some(
                    async move {
                        let symcache_result = symcache_actor.fetch(request).await;
                        (idx, symcache_result)
                    }
                    .bind_hub(Hub::new_from_top(Hub::current())),
                )
            });

        for (idx, symcache_result) in future::join_all(futures).await {
            if let Some(entry) = self.modules.get_mut(idx) {
                let (symcache, status) = match symcache_result {
                    Ok(symcache) => match symcache.parse() {
                        Ok(Some(_)) => (Some(symcache), ObjectFileStatus::Found),
                        Ok(None) => (Some(symcache), ObjectFileStatus::Missing),
                        Err(e) => (None, (&e).into()),
                    },
                    Err(e) => (None, (&*e).into()),
                };

                entry.object_info.arch = Default::default();

                if let Some(ref symcache) = symcache {
                    entry.object_info.arch = symcache.arch();
                    entry.object_info.features.merge(symcache.features());
                    entry.object_info.candidates.merge(symcache.candidates());
                }

                entry.symcache = symcache;
                entry.object_info.debug_status = status;
            }
        }
    }

    /// Fetches all the sources for the modules referenced by the `stacktraces`.
    #[tracing::instrument(skip_all)]
    pub async fn fetch_sources(
        &mut self,
        objects: ObjectsActor,
        stacktraces: &[CompleteStacktrace],
    ) {
        let mut referenced_objects = HashSet::new();
        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
                if let Some(i) =
                    self.get_module_index_by_addr(frame.raw.instruction_addr.0, frame.raw.addr_mode)
                {
                    referenced_objects.insert(i);
                }
            }
        }

        let futures = self
            .modules
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let is_used = referenced_objects.contains(&entry.module_index);
                if !is_used {
                    entry.object_info.debug_status = ObjectFileStatus::Unused;
                    entry.source_object = None;
                    return None;
                }

                let objects = objects.clone();
                let find_request = FindObject {
                    filetypes: FileType::sources(),
                    purpose: ObjectPurpose::Source,
                    scope: self.scope.clone(),
                    identifier: object_id_from_object_info(&entry.object_info.raw),
                    sources: self.sources.clone(),
                };

                Some(
                    async move {
                        let opt_object_file_meta =
                            objects.find(find_request).await.unwrap_or_default().meta;

                        let source_object = match opt_object_file_meta {
                            None => None,
                            Some(object_file_meta) => {
                                objects.fetch(object_file_meta).await.ok().and_then(|x| {
                                    SelfCell::try_new(x.data(), |b| Object::parse(unsafe { &*b }))
                                        .map(SourceObject)
                                        .ok()
                                })
                            }
                        };

                        (idx, source_object)
                    }
                    .bind_hub(Hub::new_from_top(Hub::current())),
                )
            });

        for (idx, source_object) in future::join_all(futures).await {
            if let Some(entry) = self.modules.get_mut(idx) {
                entry.source_object = source_object;

                if entry.source_object.is_some() {
                    entry.object_info.features.has_sources = true;
                }
            }
        }
    }

    /// Creates a [`ObjectDebugSession`] for each module that has a [`SourceObject`].
    ///
    /// This returns a separate HashMap purely to avoid self-referential borrowing issues.
    /// The [`ObjectDebugSession`] borrows from the [`SourceObject`] and thus they can't live within
    /// the same mutable [`ModuleLookup`].
    pub fn prepare_debug_sessions(&self) -> HashMap<usize, Option<ObjectDebugSession<'_>>> {
        self.modules
            .iter()
            .map(|entry| {
                (
                    entry.module_index,
                    entry
                        .source_object
                        .as_ref()
                        .and_then(|o| o.0.get().debug_session().ok()),
                )
            })
            .collect()
    }

    /// Look up the corresponding SymCache based on the instruction `addr`.
    pub fn lookup_symcache(
        &self,
        addr: u64,
        addr_mode: AddrMode,
    ) -> Option<SymCacheLookupResult<'_>> {
        match addr_mode {
            AddrMode::Abs => {
                for entry in self.modules.iter() {
                    let start_addr = entry.object_info.raw.image_addr.0;

                    if start_addr > addr {
                        // The debug image starts at a too high address
                        continue;
                    }

                    let size = entry.object_info.raw.image_size.unwrap_or(0);
                    if let Some(end_addr) = start_addr.checked_add(size) {
                        if end_addr < addr && size != 0 {
                            // The debug image ends at a too low address and we're also confident that
                            // end_addr is accurate (size != 0)
                            continue;
                        }
                    }

                    return Some(SymCacheLookupResult {
                        module_index: entry.module_index,
                        object_info: &entry.object_info,
                        symcache: entry.symcache.as_deref(),
                        relative_addr: entry.object_info.abs_to_rel_addr(addr),
                    });
                }
                None
            }
            AddrMode::Rel(this_module_index) => self
                .modules
                .iter()
                .find(|x| x.module_index == this_module_index)
                .map(|entry| SymCacheLookupResult {
                    module_index: entry.module_index,
                    object_info: &entry.object_info,
                    symcache: entry.symcache.as_deref(),
                    relative_addr: Some(addr),
                }),
        }
    }

    /// This looks up the source of the given line, plus `n` lines above/below.
    pub fn get_context_lines(
        &self,
        debug_sessions: &HashMap<usize, Option<ObjectDebugSession<'_>>>,
        addr: u64,
        addr_mode: AddrMode,
        abs_path: &str,
        lineno: u32,
        n: usize,
    ) -> Option<(Vec<String>, String, Vec<String>)> {
        let index = self.get_module_index_by_addr(addr, addr_mode)?;
        let session = debug_sessions.get(&index)?.as_ref()?;
        let source = session.source_by_path(abs_path).ok()??;

        let lineno = lineno as usize;
        let start_line = lineno.saturating_sub(n);
        let line_diff = lineno - start_line;

        let mut lines = source.lines().skip(start_line);
        let pre_context = (&mut lines)
            .take(line_diff.saturating_sub(1))
            .map(|x| x.to_string())
            .collect();
        let context = lines.next()?.to_string();
        let post_context = lines.take(n).map(|x| x.to_string()).collect();

        Some((pre_context, context, post_context))
    }

    // TODO:
    // * The lookup logic is mostly duplicated with `lookup_symcache`, unify the two in a followup.
    // * The lookup performs a linear scan, even though we have a sorted list (by addr), switch this
    //   to a binary search in a followup.
    fn get_module_index_by_addr(&self, addr: u64, addr_mode: AddrMode) -> Option<usize> {
        match addr_mode {
            AddrMode::Abs => {
                for entry in self.modules.iter() {
                    let start_addr = entry.object_info.raw.image_addr.0;

                    if start_addr > addr {
                        // The debug image starts at a too high address
                        continue;
                    }

                    let size = entry.object_info.raw.image_size.unwrap_or(0);
                    if let Some(end_addr) = start_addr.checked_add(size) {
                        if end_addr < addr && size != 0 {
                            // The debug image ends at a too low address and we're also confident that
                            // end_addr is accurate (size != 0)
                            continue;
                        }
                    }

                    return Some(entry.module_index);
                }
                None
            }
            AddrMode::Rel(this_module_index) => self
                .modules
                .iter()
                .find(|x| x.module_index == this_module_index)
                .map(|x| x.module_index),
        }
    }
}
