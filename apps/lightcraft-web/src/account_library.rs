//! Account project metadata and immutable originals for a single LightCraft window.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use lightcraft_engine::catalog::Catalog;

use crate::account_catalog;
use crate::account_port::{AccountPort, Start};
use crate::backend::Backend;
use crate::files::Files;
use crate::store::{content_hash, storage_key};

#[derive(Clone)]
pub(crate) struct AccountLibrary {
    pub port: AccountPort,
    originals: Rc<RefCell<BTreeMap<String, String>>>,
    cache: Rc<RefCell<BTreeMap<String, CacheState>>>,
    imports: Rc<Cell<usize>>,
    suspended: Rc<Cell<bool>>,
}

pub(crate) struct ImportGuard(Rc<Cell<usize>>);

impl Drop for ImportGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
        crate::web::request_repaint();
    }
}

enum CacheState {
    Available,
    RetryAt(f64),
}

impl AccountLibrary {
    pub fn open(port: AccountPort, request: &mut Start) -> Result<(Self, Files), String> {
        let (files, originals) = match request.bytes.take() {
            Some(bytes) => {
                let library = account_catalog::open(&bytes)?;
                (library.files, library.originals)
            }
            None => (Files::default(), BTreeMap::new()),
        };
        Ok((
            Self { port, originals: Rc::new(RefCell::new(originals)), cache: Rc::default(), imports: Rc::default(), suspended: Rc::default() },
            files,
        ))
    }

    pub async fn store_original(&self, hash: &str, bytes: &[u8]) -> Result<(), String> {
        if self.originals.borrow().contains_key(hash) {
            return Ok(());
        }
        let digest = self.port.store_original(bytes).await?;
        self.originals.borrow_mut().insert(hash.to_owned(), digest);
        Ok(())
    }

    pub async fn load_original(&self, backend: &Backend, hash: &str) -> Result<Vec<u8>, String> {
        let digest = self.originals.borrow().get(hash).cloned().ok_or("This original is absent from the account catalog.")?;
        let key = storage_key(hash);
        if let Ok(Some(bytes)) = backend.read(&key).await
            && content_hash(&bytes) == hash
        {
            self.cached(hash);
            return Ok(bytes);
        }
        let bytes = self.port.read_original(&digest).await?;
        if content_hash(&bytes) != hash {
            return Err("The account original did not match this LightCraft photo.".into());
        }
        backend.write(&key, &bytes).await?;
        self.cached(hash);
        Ok(bytes)
    }

    pub fn cached(&self, hash: &str) {
        self.cache.borrow_mut().insert(hash.into(), CacheState::Available);
    }

    pub fn is_cached(&self, hash: &str) -> bool {
        matches!(self.cache.borrow().get(hash), Some(CacheState::Available))
    }

    pub fn can_retry(&self, hash: &str) -> bool {
        !matches!(self.cache.borrow().get(hash), Some(CacheState::RetryAt(at)) if lightcraft_ui_egui::monotonic_ms() < *at)
    }

    pub fn retry_later(&self, hash: &str) {
        self.cache.borrow_mut().insert(hash.into(), CacheState::RetryAt(lightcraft_ui_egui::monotonic_ms() + 10_000.0));
    }

    pub fn capture(&self, catalog: &Catalog, files: &Files) -> Result<Vec<u8>, String> {
        account_catalog::capture(catalog, files, &self.originals.borrow())
    }

    pub fn begin_import(&self) -> Option<ImportGuard> {
        if self.suspended.get() {
            return None;
        }
        self.imports.set(self.imports.get() + 1);
        crate::web::request_repaint();
        Some(ImportGuard(self.imports.clone()))
    }

    pub fn importing(&self) -> bool {
        self.imports.get() > 0
    }

    pub fn suspend(&self, value: bool) {
        self.suspended.set(value);
    }
}
