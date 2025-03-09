use common::{Error, Result};
use proto::bazel::exec::{
    Digest, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile, Tree,
};
use std::{
    fs::{self, OpenOptions},
    io::BufReader,
    path::PathBuf,
};
use storage::{ProtoStoreExt, Store};

pub fn build_tree<S: Store>(storage: &S, basepath: &PathBuf, dir: &PathBuf) -> Result<Tree> {
    let dir_path = basepath.join(&dir);
    let entries = fs::read_dir(&dir_path).unwrap();

    let mut tree = Tree {
        root: None,
        children: vec![],
    };

    let mut root = Directory {
        files: vec![],
        directories: vec![],
        symlinks: vec![],
        node_properties: None,
    };

    for entry in entries {
        let entry = entry.unwrap();
        let entry_path = basepath.join(&entry.path());

        if entry_path.is_dir() {
            let (dir, mut children) = build_dir(storage, &basepath, &entry.path()).unwrap();
            let dir_digest = storage.write_message(&dir).unwrap();

            let dir_name = entry_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();

            tree.children.push(dir);
            tree.children.append(&mut children);
            root.directories.push(DirectoryNode {
                name: dir_name,
                digest: Some(dir_digest),
            });
            continue;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .open(&entry_path)
            .unwrap();

        let mut reader = BufReader::new(&file);
        let file_digest = storage.write_digest(reader).unwrap();

        let file_name = entry
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        root.files.push(FileNode {
            name: file_name,
            digest: Some(file_digest),
            is_executable: true,
            node_properties: None,
        });
    }

    tree.root = Some(root);
    Ok(tree)
}

fn build_dir<S: Store>(
    storage: &S,
    basepath: &PathBuf,
    dir: &PathBuf,
) -> Result<(Directory, Vec<Directory>)> {
    tracing::info!("build_dir basepath={basepath:?} dir={dir:?}");

    let dir_path = basepath.join(&dir);
    let entries = fs::read_dir(&dir_path).unwrap();

    let mut root = Directory {
        files: vec![],
        directories: vec![],
        symlinks: vec![],
        node_properties: None,
    };

    let mut children = vec![];

    for entry in entries {
        let entry = entry.unwrap();
        let entry_path = basepath.join(&entry.path());

        if entry_path.is_dir() {
            let (mut dir, mut dir_children) = build_dir(storage, &basepath, &entry.path()).unwrap();
            let dir_digest = storage.write_message(&dir).unwrap();

            let dir_name = entry_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();

            children.push(dir.clone());
            children.append(&mut dir_children);
            root.directories.push(DirectoryNode {
                name: dir_name,
                digest: Some(dir_digest),
            });
            continue;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .open(&entry_path)
            .unwrap();

        let mut reader = BufReader::new(&file);
        let file_digest = storage.write_digest(reader).unwrap();

        let file_name = entry
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        root.files.push(FileNode {
            name: file_name,
            digest: Some(file_digest),
            is_executable: true,
            node_properties: None,
        });
    }

    tracing::info!("OUTPUT FOR {dir_path:?} IS {root:#?} AND {children:#?}");

    Ok((root, children))
}


#[cfg(test)]
mod test {
    use super::*;
    use proto::bazel::exec::Digest;
    use std::fs::{self, File};
    use storage::mem::MemStore;
    use tempdir::TempDir;

    #[test]
    fn test_build_tree() {
        let env = TestEnv::setup();
        let mut store = MemStore::new();

        let output = build_tree(&mut store, &env.execroot_path, &env.output_path).unwrap();

        println!("{output:#?}");
        assert!(false);

        // assert_eq!(
        //     output,
        //     OutputDir {
        //         path: PathBuf::from("a/b/dir"),
        //         files: vec![an_output_file("a/b/dir/bar")],
        //         dirs: vec![OutputDir {
        //             path: PathBuf::from("a/b/dir/foo"),
        //             files: vec![an_output_file("a/b/dir/foo/baz")],
        //             dirs: vec![],
        //         }]
        //     }
        // )
    }

    fn an_output_file(path: &str) -> OutputFile {
        OutputFile {
            path: path.to_owned(),
            digest: Some(empty_digest()),
            is_executable: false,
            node_properties: None,
            contents: vec![],
        }
    }

    fn empty_digest() -> Digest {
        Digest {
            hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            size_bytes: 0,
        }
    }

    pub struct TestEnv {
        pub execroot_path: PathBuf,
        pub output_path: PathBuf,
        // Deletes the temp directory when dropped.
        #[allow(dead_code)]
        temp: TempDir,
    }

    impl TestEnv {
        pub fn setup() -> Self {
            let root = TempDir::new("test").unwrap();
            let root_path = root.path().to_owned();

            let output_dir_path = root_path.join("a").join("b").join("dir");
            fs::create_dir_all(&output_dir_path).unwrap();

            let bar_path = output_dir_path.join("bar");
            File::create(&bar_path).unwrap();

            let foo_path = output_dir_path.join("foo");
            fs::create_dir(&foo_path).unwrap();

            let baz_path = foo_path.join("baz");
            File::create(&baz_path).unwrap();

            Self {
                temp: root,
                execroot_path: root_path,
                output_path: PathBuf::from("a/b/dir"),
            }
        }
    }
}
