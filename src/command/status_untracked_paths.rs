use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

use git_internal::internal::index::Index;

pub(crate) struct TrackedPaths {
    files: Vec<PathBuf>,
    top_level_dirs: HashSet<PathBuf>,
}

impl TrackedPaths {
    pub(crate) fn from_index(index: &Index) -> Self {
        let files = index.tracked_files();
        let top_level_dirs = files
            .iter()
            .filter_map(|path| top_level_dir(path))
            .collect();
        Self {
            files,
            top_level_dirs,
        }
    }

    pub(crate) fn files(&self) -> &[PathBuf] {
        &self.files
    }

    pub(crate) fn has_descendant(&self, dir: &Path) -> bool {
        if is_top_level_path(dir) {
            return self.top_level_dirs.contains(dir);
        }
        self.files.iter().any(|file| file.starts_with(dir))
    }
}

pub(crate) fn collapse_untracked_directories(
    untracked_files: Vec<PathBuf>,
    tracked: &TrackedPaths,
) -> Vec<PathBuf> {
    if untracked_files.is_empty() {
        return untracked_files;
    }

    let mut dir_files: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut root_files: Vec<PathBuf> = Vec::new();
    for file in &untracked_files {
        let components: Vec<_> = file.components().collect();
        if components.len() > 1 {
            let top_dir = PathBuf::from(components[0].as_os_str());
            dir_files.entry(top_dir).or_default().push(file.clone());
        } else {
            root_files.push(file.clone());
        }
    }

    let mut result: BTreeSet<PathBuf> = BTreeSet::new();
    result.extend(root_files);
    for (dir, files) in dir_files {
        if tracked.has_descendant(&dir) {
            result.extend(files);
        } else {
            result.insert(directory_marker(&dir));
        }
    }

    result.into_iter().collect()
}

pub(crate) fn sort_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn is_top_level_path(path: &Path) -> bool {
    path.components().count() == 1
}

pub(crate) fn directory_marker(path: &Path) -> PathBuf {
    let mut display = path.display().to_string();
    if !display.ends_with('/') {
        display.push('/');
    }
    PathBuf::from(display)
}

fn top_level_dir(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let first = PathBuf::from(components.next()?.as_os_str());
    components.next().map(|_| first)
}
