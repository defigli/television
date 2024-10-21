use color_eyre::owo_colors::OwoColorize;
use devicons::FileIcon;
use ignore::{overrides::OverrideBuilder, DirEntry};
use nucleo::{
    pattern::{CaseMatching, Normalization},
    Config, Nucleo,
};
use parking_lot::Mutex;
use std::{collections::HashSet, sync::Arc};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};
use tracing::debug;

use crate::{
    entry::Entry,
    fuzzy::MATCHER,
    previewers::PreviewType,
    utils::files::{walk_builder, DEFAULT_NUM_THREADS},
};

use crate::channels::OnAir;

pub struct Channel {
    matcher: Nucleo<DirEntry>,
    last_pattern: String,
    result_count: u32,
    total_count: u32,
    running: bool,
    icon: FileIcon,
    crawl_handle: JoinHandle<()>,
    entry_cache: Arc<Mutex<HashSet<String>>>,
    // TODO: implement cache validation/invalidation
    cache_valid: bool,
}

impl Channel {
    pub fn new() -> Self {
        let matcher = Nucleo::new(
            Config::DEFAULT.match_paths(),
            Arc::new(|| {}),
            None,
            1,
        );
        let entry_cache = Arc::new(Mutex::new(HashSet::new()));
        // start loading files in the background
        // PERF: store the results somewhere in a cache
        let crawl_handle = tokio::spawn(crawl_for_repos(
            std::env::home_dir().expect("Could not get home directory"),
            matcher.injector(),
            entry_cache.clone(),
        ));
        Channel {
            matcher,
            last_pattern: String::new(),
            result_count: 0,
            total_count: 0,
            running: false,
            icon: FileIcon::from("git"),
            crawl_handle,
            entry_cache,
            cache_valid: false,
        }
    }

    const MATCHER_TICK_TIMEOUT: u64 = 2;
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl OnAir for Channel {
    fn find(&mut self, pattern: &str) {
        if pattern != self.last_pattern {
            self.matcher.pattern.reparse(
                0,
                pattern,
                CaseMatching::Smart,
                Normalization::Smart,
                pattern.starts_with(&self.last_pattern),
            );
            self.last_pattern = pattern.to_string();
        }
    }

    fn results(&mut self, num_entries: u32, offset: u32) -> Vec<Entry> {
        let status = self.matcher.tick(Self::MATCHER_TICK_TIMEOUT);
        let snapshot = self.matcher.snapshot();
        if status.changed {
            self.result_count = snapshot.matched_item_count();
            self.total_count = snapshot.item_count();
        }
        self.running = status.running;
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        let icon = self.icon;

        snapshot
            .matched_items(
                offset
                    ..(num_entries + offset)
                        .min(snapshot.matched_item_count()),
            )
            .map(move |item| {
                snapshot.pattern().column_pattern(0).indices(
                    item.matcher_columns[0].slice(..),
                    &mut matcher,
                    &mut indices,
                );
                indices.sort_unstable();
                indices.dedup();
                let indices = indices.drain(..);

                let path = item.matcher_columns[0].to_string();
                Entry::new(path.clone(), PreviewType::Directory)
                    .with_name_match_ranges(
                        indices.map(|i| (i, i + 1)).collect(),
                    )
                    .with_icon(icon)
            })
            .collect()
    }

    fn get_result(&self, index: u32) -> Option<Entry> {
        let snapshot = self.matcher.snapshot();
        snapshot.get_matched_item(index).map(|item| {
            let path = item.matcher_columns[0].to_string();
            Entry::new(path.clone(), PreviewType::Directory)
                .with_icon(self.icon)
        })
    }

    fn result_count(&self) -> u32 {
        self.result_count
    }

    fn total_count(&self) -> u32 {
        self.total_count
    }

    fn running(&self) -> bool {
        self.running
    }

    fn shutdown(&self) {
        debug!("Shutting down git repos channel");
        self.crawl_handle.abort();
    }
}

#[allow(clippy::unused_async)]
async fn crawl_for_repos(
    starting_point: std::path::PathBuf,
    injector: nucleo::Injector<DirEntry>,
    entry_cache: Arc<Mutex<HashSet<String>>>,
) {
    let mut walker_overrides_builder = OverrideBuilder::new(&starting_point);
    walker_overrides_builder.add(".git").unwrap();
    let walker = walk_builder(
        &starting_point,
        *DEFAULT_NUM_THREADS,
        Some(walker_overrides_builder.build().unwrap()),
    )
    .build_parallel();

    walker.run(|| {
        let injector = injector.clone();
        let entry_cache = entry_cache.clone();
        Box::new(move |result| {
            if let Ok(entry) = result {
                if entry.file_type().unwrap().is_dir() {
                    // if the dir is already in cache, skip it
                    let path = entry.path().to_string_lossy().to_string();
                    if entry_cache.lock().contains(&path) {
                        return ignore::WalkState::Skip;
                    }
                    // if the entry is a .git directory, add its parent to the list
                    // of git repos and cache it
                    if entry.path().ends_with(".git") {
                        let parent_path = entry
                            .path()
                            .parent()
                            .unwrap()
                            .to_string_lossy()
                            .to_string();
                        debug!("Found git repo: {:?}", parent_path);
                        let _ = injector.push(entry, |_e, cols| {
                            cols[0] = parent_path.clone().into();
                        });
                        entry_cache.lock().insert(parent_path);
                        return ignore::WalkState::Skip;
                    }
                }
            }
            ignore::WalkState::Continue
        })
    });
}