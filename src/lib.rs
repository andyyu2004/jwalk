//! Fast recursive directory walk.
//!
//! - Walk is performed in parallel using rayon
//! - Results are streamed in sorted order
//! - Custom sorting/filtering if needed
//!
//! This crate is inspired by both [`walkdir`](https://crates.io/crates/walkdir)
//! and [`ignore`](https://crates.io/crates/ignore). It attempts to combine the
//! parallelism of `ignore` with `walkdir`s streaming iterator API.
//!
//! # Example
//!
//! Recursively iterate over the "foo" directory sorting by name:
//!
//! ```no_run
//! # use std::io::Error;
//! use jwalk::{Sort, WalkDir};
//!
//! # fn try_main() -> Result<(), Error> {
//! for entry in WalkDir::new("foo").sort(Some(Sort::Name)) {
//!   println!("{}", entry?.path().display());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Why would you use this crate?
//!
//! Speed is the main reason. The following benchmarks walk linux's source code
//! under various conditions. Run these benchmarks yourself using `cargo bench`.
//!
//! | Crate   | Options                        | Time      |
//! |---------|--------------------------------|-----------|
//! | jwalk   | unsorted, parallel             | 60.811 ms |
//! | jwalk   | sorted, parallel               | 61.445 ms |
//! | jwalk   | sorted, parallel, metadata     | 100.95 ms |
//! | jwalk   | unsorted, parallel (2 threads) | 99.998 ms |
//! | jwalk   | unsorted, serial               | 168.68 ms |
//! | jwalk   | sorted, parallel, first 100    | 9.9794 ms |
//! | ignore  | unsorted, parallel             | 74.251 ms |
//! | ignore  | sorted, parallel               | 99.336 ms |
//! | ignore  | sorted, parallel, metadata     | 134.26 ms |
//! | walkdir | unsorted                       | 162.09 ms |
//! | walkdir | sorted                         | 200.09 ms |
//! | walkdir | sorted, metadata               | 422.74 ms |
//!
//! Note in particular that this crate is fast when you want streamed sorted
//! results. Also note that even when used in single thread mode this crate is
//! very close to `walkdir` in performance.
//!
//! This crate's parallelism happens at `fs::read_dir` granularity. If you are
//! walking many files in a single directory it won't help. On the other hand if
//! you are walking a hierarchy with multiple folders then it can help a lot.
//!
//! Also note that even though the `ignore` crate has similar performance to
//! this crate is has much worse latency when you want sorted results. This
//! crate will start streaming sorted results right away, while with `ignore`
//! you'll need to wait until the entire walk finishes before you can sort and
//! start processing the results in sorted order.
//!
//! # Why wouldn't you use this crate?
//!
//! Directory traversal is already pretty fast with existing crates. `walkdir` in
//! particular is great if you need a single threaded solution.
//!
//! This crate processes each `fs::read_dir` as a single unit. Reading all
//! entries and converting them into its own `DirEntry` representation. This
//! representation is fairly lightweight, but if you have an extremely wide or
//! deep directory structure it might cause problems holding too many
//! `DirEntry`s in memory at once. The concern here is memory, not open file
//! descriptors. This crate only keeps one open file descriptor per rayon
//! thread.

mod core;

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::{DirEntryIter, ReadDir};

pub use crate::core::{DirEntry, ReadDirSpec};

/// Builder to create an iterator for walking a directory.
pub struct WalkDir {
  root: PathBuf,
  options: WalkDirOptions,
}

/// Per directory sort options. If you need more flexibility use
/// [`process_entries`](struct.WalkDir.html#method.process_entries).
#[derive(Clone)]
pub enum Sort {
  Name,
  Access,
  Creation,
  Modification,
}

struct WalkDirOptions {
  sort: Option<Sort>,
  max_depth: usize,
  skip_hidden: bool,
  num_threads: usize,
  preload_metadata: bool,
  process_entries: Option<Arc<Fn(&mut Vec<Result<DirEntry>>) + Send + Sync>>,
}

impl WalkDir {
  /// Create a builder for a recursive directory iterator starting at the file
  /// path root. If root is a directory, then it is the first item yielded by
  /// the iterator. If root is a file, then it is the first and only item
  /// yielded by the iterator.
  pub fn new<P: AsRef<Path>>(root: P) -> Self {
    WalkDir {
      root: root.as_ref().to_path_buf(),
      options: WalkDirOptions {
        sort: None,
        max_depth: ::std::usize::MAX,
        num_threads: 0,
        skip_hidden: true,
        preload_metadata: false,
        process_entries: None,
      },
    }
  }

  /// Set the maximum depth of entries yield by the iterator.
  ///
  /// The smallest depth is `0` and always corresponds to the path given to the
  /// `new` function on this type. Its direct descendents have depth `1`, and
  /// their descendents have depth `2`, and so on.
  ///
  /// Note that a depth < 2 will automatically change `thread_count` to 1.
  /// `jwalks` parrallelism happens at the `fs::read_dir` level, so it only
  /// makes sense to use multiple threads when reading more then one directory.
  pub fn max_depth(mut self, depth: usize) -> Self {
    self.options.max_depth = depth;
    if depth == 1 {
      self.options.num_threads = 1;
    }
    self
  }

  /// Sort entries per directory. Use
  /// [`process_entries`](struct.WalkDir.html#method.process_entries) for custom
  /// sorting or filtering.
  pub fn sort(mut self, sort: Option<Sort>) -> Self {
    self.options.sort = sort;
    self
  }

  /// Number of threads to use:
  ///
  /// - `0` Use rayon global pool.
  /// - `1` Perform walk on calling thread.
  /// - `n > 1` Construct a new rayon ThreadPool to perform the walk.
  pub fn num_threads(mut self, n: usize) -> Self {
    self.options.num_threads = n;
    self
  }

  /// Skip hidden entries as determined by leading `.` in file name.
  /// Enabled by default.
  pub fn skip_hidden(mut self, skip_hidden: bool) -> Self {
    self.options.skip_hidden = skip_hidden;
    self
  }

  /// Preload metadata before returning entries. This can improve performance if
  /// you need to access the metadata of each yielded entry. The metadata
  /// loading is done ahead of time in rayon's thread pool.
  pub fn preload_metadata(mut self, preload_metadata: bool) -> Self {
    self.options.preload_metadata = preload_metadata;
    self
  }

  /// Set a function to process entries before they are yeilded through the walk
  /// iterator. This function can filter/sort the given list of entries. It can also make
  /// the walk skip descending into particular directories by calling
  /// [`entry.set_children_spec(None)`](struct.DirEntry.html#method.children_spec)
  /// on them.
  pub fn process_entries<F>(mut self, process_by: F) -> Self
  where
    F: Fn(&mut Vec<Result<DirEntry>>) + Send + Sync + 'static,
  {
    self.options.process_entries = Some(Arc::new(process_by));
    self
  }
}

impl IntoIterator for WalkDir {
  type Item = Result<DirEntry>;
  type IntoIter = DirEntryIter;

  fn into_iter(self) -> DirEntryIter {
    let sort = self.options.sort;
    let num_threads = self.options.num_threads;
    let skip_hidden = self.options.skip_hidden;
    let max_depth = self.options.max_depth;
    let preload_metadata = self.options.preload_metadata;
    let process_entries = self.options.process_entries.clone();

    let dir_entry_iter = core::walk(&self.root, num_threads, move |read_dir_spec| {
      let depth = read_dir_spec.depth + 1;
      let mut dir_entry_results: Vec<_> = fs::read_dir(&read_dir_spec.path)?
        .filter_map(|dir_entry_result| {
          let dir_entry = match dir_entry_result {
            Ok(dir_entry) => dir_entry,
            Err(err) => return Some(Err(err)),
          };

          let file_name = dir_entry.file_name();
          if skip_hidden && is_hidden(&file_name) {
            return None;
          }

          let file_type = dir_entry.file_type();

          let metadata = if preload_metadata {
            Some(dir_entry.metadata())
          } else {
            None
          };

          let children_spec = match file_type {
            Ok(file_type) => {
              if file_type.is_dir() && depth < max_depth {
                let path = read_dir_spec.path.join(dir_entry.file_name());
                Some(Arc::new(ReadDirSpec::new(path, depth, None)))
              } else {
                None
              }
            }
            Err(_) => None,
          };

          Some(Ok(DirEntry::new(
            depth,
            file_name,
            file_type,
            metadata,
            Some(read_dir_spec.clone()),
            children_spec,
          )))
        })
        .collect();

      sort
        .as_ref()
        .map(|sort| sort.perform_sort(&mut dir_entry_results));

      process_entries.as_ref().map(|process_entries| {
        process_entries(&mut dir_entry_results);
      });

      Ok(ReadDir::new(dir_entry_results))
    });

    dir_entry_iter
  }
}

impl Sort {
  fn perform_sort(&self, dir_entry_results: &mut Vec<Result<DirEntry>>) {
    dir_entry_results.sort_by(|a, b| match (a, b) {
      (Ok(a), Ok(b)) => a.file_name().cmp(b.file_name()),
      (Ok(_), Err(_)) => Ordering::Less,
      (Err(_), Ok(_)) => Ordering::Greater,
      (Err(_), Err(_)) => Ordering::Equal,
    });
  }
}

impl Clone for WalkDirOptions {
  fn clone(&self) -> WalkDirOptions {
    WalkDirOptions {
      sort: None,
      max_depth: self.max_depth,
      num_threads: self.num_threads,
      skip_hidden: self.skip_hidden,
      preload_metadata: self.preload_metadata,
      process_entries: self.process_entries.clone(),
    }
  }
}

fn is_hidden(file_name: &OsStr) -> bool {
  file_name
    .to_str()
    .map(|s| s.starts_with("."))
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {

  use super::*;
  use walkdir;

  fn test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/assets/test_dir")
  }

  fn local_paths(walk_dir: WalkDir) -> Vec<String> {
    let test_dir = test_dir();
    walk_dir
      .into_iter()
      .map(|each_result| {
        let each_entry = each_result.unwrap();
        let path = each_entry.path().to_path_buf();
        let path = path.strip_prefix(&test_dir).unwrap().to_path_buf();
        let mut path_string = path.to_str().unwrap().to_string();
        path_string.push_str(&format!(" ({})", each_entry.depth()));
        path_string
      })
      .collect()
  }

  #[test]
  fn test_walk() {
    let paths = local_paths(WalkDir::new(test_dir()));
    assert!(paths.contains(&"b.txt (1)".to_string()));
    assert!(paths.contains(&"group 1 (1)".to_string()));
    assert!(paths.contains(&"group 1/d.txt (2)".to_string()));
  }

  #[test]
  fn test_sort_by_name_single_thread() {
    let paths = local_paths(
      WalkDir::new(test_dir())
        .num_threads(1)
        .sort(Some(Sort::Name)),
    );
    assert!(
      paths
        == vec![
          " (0)",
          "a.txt (1)",
          "b.txt (1)",
          "c.txt (1)",
          "group 1 (1)",
          "group 1/d.txt (2)",
          "group 2 (1)",
          "group 2/e.txt (2)",
        ]
    );
  }

  #[test]
  fn test_sort_by_name_rayon_pool_global() {
    let paths = local_paths(WalkDir::new(test_dir()).sort(Some(Sort::Name)));
    assert!(
      paths
        == vec![
          " (0)",
          "a.txt (1)",
          "b.txt (1)",
          "c.txt (1)",
          "group 1 (1)",
          "group 1/d.txt (2)",
          "group 2 (1)",
          "group 2/e.txt (2)",
        ]
    );
  }

  #[test]
  fn test_sort_by_name_rayon_pool_2_threads() {
    let paths = local_paths(
      WalkDir::new(test_dir())
        .num_threads(2)
        .sort(Some(Sort::Name)),
    );
    assert!(
      paths
        == vec![
          " (0)",
          "a.txt (1)",
          "b.txt (1)",
          "c.txt (1)",
          "group 1 (1)",
          "group 1/d.txt (2)",
          "group 2 (1)",
          "group 2/e.txt (2)",
        ]
    );
  }

  #[test]
  fn test_see_hidden_files() {
    let paths = local_paths(
      WalkDir::new(test_dir())
        .skip_hidden(false)
        .sort(Some(Sort::Name)),
    );
    assert!(paths.contains(&"group 2/.hidden_file.txt (2)".to_string()));
  }

  #[test]
  fn test_max_depth() {
    let paths = local_paths(WalkDir::new(test_dir()).max_depth(1).sort(Some(Sort::Name)));
    assert!(
      paths
        == vec![
          " (0)",
          "a.txt (1)",
          "b.txt (1)",
          "c.txt (1)",
          "group 1 (1)",
          "group 2 (1)",
        ]
    );
  }

  #[test]
  fn test_walk_file() {
    let walk_dir = WalkDir::new(test_dir().join("a.txt"));
    let mut iter = walk_dir.into_iter();
    assert!(iter.next().unwrap().unwrap().file_name().to_str().unwrap() == "a.txt");
    assert!(iter.next().is_none());
  }

  #[test]
  fn test_walk_root() {
    let mut iter = walkdir::WalkDir::new("/").max_depth(1).into_iter();
    assert!(iter.next().unwrap().unwrap().file_name() == "/");
  }

}
