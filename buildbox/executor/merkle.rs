// use common::{Error, Result};
// use std::cell::{Ref, RefCell};
// use std::fs::{self, OpenOptions};
// use std::io::BufReader;
// use std::path::Path;
// use std::rc::Rc;
// use std::{collections::VecDeque, path::PathBuf};
// use storage::ProtoStoreExt;
// use storage::Store;

// use crate::{OutputDir, OutputFile};

// // pub fn build_tree<S: Store>(
// //     store: &S,
// //     basepath: &PathBuf,
// //     output_rel_path: &PathBuf,
// // ) -> Result<OutputTree> {
// //     todo!()
// // }

// // pub fn build_output_dir<S: Store>(
// //     store: &S,
// //     basepath: &PathBuf,
// //     output_rel_path: &PathBuf,
// // ) -> Result<(OutputDir, Vec<OutputDir>)> {
// //     let mut out = OutputDir {
// //         path: output_rel_path.clone(),
// //         files: vec![],
// //         dirs: vec![],
// //     };

// //     let mut children = vec![];

// //     let abs_path = basepath.join(&out.path);

// //     let mut entries = fs::read_dir(&abs_path).map_err(Error::io)?;

// //     for entry in entries {
// //         let entry = entry.map_err(Error::io)?;

// //         let abs_entry_path = entry.path();
// //         let rel_entry_path = abs_entry_path
// //             .strip_prefix(basepath)
// //             .map_err(Error::boxed)?
// //             .to_owned();

// //         if abs_entry_path.is_dir() {
// //             let (dir, dir_children) = build_output_dir(&mut store, &basepath, &rel_entry_path)?;
// //             out.dirs.push(dir);
// //             children.append(&mut dir_children);
// //             continue;
// //         }

// //         let mut file = OpenOptions::new()
// //             .read(true)
// //             .open(abs_entry_path)
// //             .map_err(Error::io)?;

// //         let mut reader = BufReader::new(&file);
// //         let file_digest = store.write_digest(reader)?;

// //         out.files.push(OutputFile {
// //             path: rel_entry_path,
// //             digest: file_digest,
// //         });
// //     }

// //     Ok((out, children))
// // }

// #[derive(Clone)]
// struct State {
//     path: PathBuf,
//     dirs: Vec<Rc<RefCell<State>>>,
//     files: Vec<OutputFile>,
// }

// impl State {
//     // TODO: Make iterative, not recursive.
//     fn into_output_dir(self) -> OutputDir {
//         let mut ret = OutputDir {
//             path: self.path,
//             dirs: vec![],
//             files: self.files,
//         };

//         for substate in &self.dirs {
//             let substate = substate.as_ref().clone().into_inner();
//             ret.dirs.push(substate.into_output_dir());
//         }

//         ret
//     }
// }

// pub fn merkle<S: Store>(
//     store: &S,
//     basepath: &PathBuf,
//     output_rel_path: &PathBuf,
// ) -> Result<OutputDir> {
//     let root_output = Rc::new(RefCell::new(State {
//         path: output_rel_path.to_owned(),
//         dirs: vec![],
//         files: vec![],
//     }));

//     let mut stack = VecDeque::new();
//     stack.push_back(root_output.clone());

//     while let Some(state) = stack.pop_front() {
//         let mut state = state.borrow_mut();

//         let abs_curr_path = basepath.join(&state.path);
//         let path = basepath.join(&state.path);

//         let mut entries = fs::read_dir(&abs_curr_path).map_err(Error::io)?;

//         for entry in entries {
//             let entry = entry.map_err(Error::io)?;

//             let abs_entry_path = entry.path();
//             let rel_entry_path = abs_entry_path
//                 .strip_prefix(basepath)
//                 .map_err(Error::boxed)?
//                 .to_owned();

//             if abs_entry_path.is_dir() {
//                 let subtree_state = Rc::new(RefCell::new(State {
//                     path: rel_entry_path,
//                     files: vec![],
//                     dirs: vec![],
//                 }));
//                 stack.push_front(subtree_state.clone());
//                 state.dirs.push(subtree_state.clone());
//                 continue;
//             }

//             let mut file = OpenOptions::new()
//                 .read(true)
//                 .open(abs_entry_path)
//                 .map_err(Error::io)?;

//             let mut reader = BufReader::new(&file);
//             let file_digest = store.write_digest(reader)?;

//             state.files.push(OutputFile {
//                 path: rel_entry_path,
//                 digest: file_digest,
//             });
//         }
//     }

//     let inner = root_output.as_ref().clone().into_inner();
//     Ok(inner.into_output_dir())
// }

// #[cfg(test)]
// mod test {
//     use super::*;
//     use proto::bazel::exec::Digest;
//     use std::fs::{self, File};
//     use storage::mem::MemStore;
//     use tempdir::TempDir;

//     #[test]
//     fn test_merkle() {
//         let env = TestEnv::setup();
//         let mut store = MemStore::new();

//         let output = merkle(&mut store, &env.execroot_path, &env.output_path).unwrap();

//         assert_eq!(
//             output,
//             OutputDir {
//                 path: PathBuf::from("a/b/dir"),
//                 files: vec![an_output_file("a/b/dir/bar")],
//                 dirs: vec![OutputDir {
//                     path: PathBuf::from("a/b/dir/foo"),
//                     files: vec![an_output_file("a/b/dir/foo/baz")],
//                     dirs: vec![],
//                 }]
//             }
//         )
//     }

//     fn an_output_file(path: &str) -> OutputFile {
//         OutputFile {
//             path: PathBuf::from(path),
//             digest: empty_digest(),
//         }
//     }

//     fn empty_digest() -> Digest {
//         Digest {
//             hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
//             size_bytes: 0,
//         }
//     }

//     pub struct TestEnv {
//         pub execroot_path: PathBuf,
//         pub output_path: PathBuf,
//         // Deletes the temp directory when dropped.
//         #[allow(dead_code)]
//         temp: TempDir,
//     }

//     impl TestEnv {
//         pub fn setup() -> Self {
//             let root = TempDir::new("test").unwrap();
//             let root_path = root.path().to_owned();

//             let output_dir_path = root_path.join("a").join("b").join("dir");
//             fs::create_dir_all(&output_dir_path).unwrap();

//             let bar_path = output_dir_path.join("bar");
//             File::create(&bar_path).unwrap();

//             let foo_path = output_dir_path.join("foo");
//             fs::create_dir(&foo_path).unwrap();

//             let baz_path = foo_path.join("baz");
//             File::create(&baz_path).unwrap();

//             Self {
//                 temp: root,
//                 execroot_path: root_path,
//                 output_path: PathBuf::from("a/b/dir"),
//             }
//         }
//     }
// }
