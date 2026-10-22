//! An in-memory SFTP server filesystem for the tests: folders, files and permission bits.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

pub const HOME: &str = "/home/kit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir { mode: u32 },
    File { data: Vec<u8>, mode: u32 },
}

/// Paths to nodes; shared so the test can look inside.
pub type Tree = Arc<Mutex<BTreeMap<String, Node>>>;

pub fn tree() -> Tree {
    let mut nodes = BTreeMap::new();
    for dir in ["/", "/home", HOME] {
        nodes.insert(dir.to_owned(), Node::Dir { mode: 0o755 });
    }
    Arc::new(Mutex::new(nodes))
}

pub struct MemFs {
    tree: Tree,
    /// Handle to path; folders also keep whether their listing was sent.
    handles: HashMap<String, (String, bool)>,
    next: u32,
}

impl MemFs {
    pub fn new(tree: Tree) -> Self {
        Self {
            tree,
            handles: HashMap::new(),
            next: 0,
        }
    }

    fn handle(&mut self, path: String) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.handles.insert(handle.clone(), (path, false));
        handle
    }

    fn path(&self, handle: &str) -> Result<String, StatusCode> {
        self.handles
            .get(handle)
            .map(|(p, _)| p.clone())
            .ok_or(StatusCode::Failure)
    }

    fn attrs(&self, path: &str) -> Result<FileAttributes, StatusCode> {
        let tree = self.tree.lock().unwrap();
        let mut attrs = FileAttributes::empty();
        match tree.get(&normalize(path)).ok_or(StatusCode::NoSuchFile)? {
            Node::Dir { mode } => {
                attrs.permissions = Some(*mode);
                attrs.set_dir(true);
            }
            Node::File { data, mode } => {
                attrs.permissions = Some(*mode);
                attrs.set_regular(true);
                attrs.size = Some(data.len() as u64);
            }
        }
        Ok(attrs)
    }
}

/// Absolute path without `.`, `..` or a trailing slash; relative paths start at [`HOME`].
pub fn normalize(path: &str) -> String {
    let full = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{HOME}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((parent, _)) => parent.into(),
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

impl russh_sftp::server::Handler for MemFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = normalize(&filename);
        {
            let mut tree = self.tree.lock().unwrap();
            match tree.get_mut(&path) {
                Some(Node::Dir { .. }) => return Err(StatusCode::Failure),
                Some(Node::File { data, .. }) if flags.contains(OpenFlags::TRUNCATE) => {
                    data.clear()
                }
                Some(_) => {}
                None if flags.contains(OpenFlags::CREATE) => {
                    if !matches!(tree.get(&parent(&path)), Some(Node::Dir { .. })) {
                        return Err(StatusCode::NoSuchFile);
                    }
                    tree.insert(
                        path.clone(),
                        Node::File {
                            data: Vec::new(),
                            mode: 0o644,
                        },
                    );
                }
                None => return Err(StatusCode::NoSuchFile),
            }
        }
        let handle = self.handle(path);
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let path = self.path(&handle)?;
        let tree = self.tree.lock().unwrap();
        let Some(Node::File { data, .. }) = tree.get(&path) else {
            return Err(StatusCode::Failure);
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= data.len() {
            return Err(StatusCode::Eof);
        }
        let end = data.len().min(start + len as usize);
        Ok(Data {
            id,
            data: data[start..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        bytes: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let path = self.path(&handle)?;
        let mut tree = self.tree.lock().unwrap();
        let Some(Node::File { data, .. }) = tree.get_mut(&path) else {
            return Err(StatusCode::Failure);
        };
        let start = offset as usize;
        if data.len() < start + bytes.len() {
            data.resize(start + bytes.len(), 0);
        }
        data[start..start + bytes.len()].copy_from_slice(&bytes);
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs {
            id,
            attrs: self.attrs(&path)?,
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs {
            id,
            attrs: self.attrs(&path)?,
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let path = self.path(&handle)?;
        Ok(Attrs {
            id,
            attrs: self.attrs(&path)?,
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut tree = self.tree.lock().unwrap();
        let node = tree
            .get_mut(&normalize(&path))
            .ok_or(StatusCode::NoSuchFile)?;
        if let Some(new) = attrs.permissions {
            match node {
                Node::Dir { mode } | Node::File { mode, .. } => *mode = new & 0o7777,
            }
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let path = normalize(&path);
        if !matches!(self.tree.lock().unwrap().get(&path), Some(Node::Dir { .. })) {
            return Err(StatusCode::NoSuchFile);
        }
        let handle = self.handle(path);
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let (dir, sent) = self
            .handles
            .get(&handle)
            .cloned()
            .ok_or(StatusCode::Failure)?;
        if sent {
            return Err(StatusCode::Eof);
        }
        self.handles.insert(handle, (dir.clone(), true));
        let prefix = if dir == "/" {
            "/".to_owned()
        } else {
            format!("{dir}/")
        };
        let children: Vec<String> = self
            .tree
            .lock()
            .unwrap()
            .keys()
            .filter(|p| p.starts_with(&prefix) && p.len() > prefix.len())
            .filter(|p| !p[prefix.len()..].contains('/'))
            .cloned()
            .collect();
        let mut files = Vec::new();
        for child in children {
            let attrs = self.attrs(&child)?;
            files.push(File::new(&child[prefix.len()..], attrs));
        }
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let mut tree = self.tree.lock().unwrap();
        match tree.get(&normalize(&filename)) {
            Some(Node::File { .. }) => {
                tree.remove(&normalize(&filename));
                Ok(ok(id))
            }
            Some(Node::Dir { .. }) => Err(StatusCode::Failure),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = normalize(&path);
        let mut tree = self.tree.lock().unwrap();
        if tree.contains_key(&path) {
            return Err(StatusCode::Failure);
        }
        tree.insert(path, Node::Dir { mode: 0o755 });
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let path = normalize(&path);
        let mut tree = self.tree.lock().unwrap();
        let prefix = format!("{path}/");
        if tree.keys().any(|p| p.starts_with(&prefix)) {
            return Err(StatusCode::Failure);
        }
        tree.remove(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(normalize(&path))],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let (old, new) = (normalize(&oldpath), normalize(&newpath));
        let mut tree = self.tree.lock().unwrap();
        let prefix = format!("{old}/");
        let moved: Vec<String> = tree
            .keys()
            .filter(|p| **p == old || p.starts_with(&prefix))
            .cloned()
            .collect();
        if moved.is_empty() {
            return Err(StatusCode::NoSuchFile);
        }
        for path in moved {
            let node = tree.remove(&path).expect("listed above");
            tree.insert(format!("{new}{}", &path[old.len()..]), node);
        }
        Ok(ok(id))
    }
}
