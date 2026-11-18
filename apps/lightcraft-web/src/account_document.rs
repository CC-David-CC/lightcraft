//! Versioned account snapshots of the editor's native catalog.

use lightcraft_engine::catalog::PhotoId;
use lightcraft_ui_egui::LightcraftApp;
use lightcraft_ui_egui::state::Dialog;
use serde_json::{Value, json};
use wasm_bindgen::JsValue;

use crate::account_library::AccountLibrary;
use crate::account_port::{Start, field, string};
use crate::files::Files;

const DOCUMENT_ID: &str = "1";

#[derive(PartialEq, Eq)]
struct Save {
    ticket: u64,
    revision: u64,
    path: String,
}

impl Save {
    fn packet(&self) -> Value {
        json!({
            "ticket":self.ticket.to_string(),
            "revision":self.revision.to_string(),
            "documentId":DOCUMENT_ID,
            "path":self.path,
        })
    }

    fn read(packet: &JsValue) -> Option<Self> {
        Some(Self { ticket: integer(packet, "ticket")?, revision: integer(packet, "revision")?, path: string(packet, "path")? })
    }
}

fn integer(packet: &JsValue, name: &str) -> Option<u64> {
    let value = string(packet, name)?;
    let number = value.parse::<u64>().ok()?;
    (number.to_string() == value).then_some(number)
}

fn valid_path(path: &str) -> bool {
    path.len() <= 2048
        && path.rsplit_once('.').is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("lightcraft"))
        && !path.chars().any(|c| c == '\\' || c.is_control())
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

pub(crate) struct AccountDocument {
    pub library: AccountLibrary,
    path: Option<String>,
    local: u64,
    confirmed: u64,
    source_revision: (u64, u64),
    ticket: u64,
    pending: Option<Save>,
    last_status: Option<Value>,
    lease: Option<String>,
    finalized: bool,
}

impl AccountDocument {
    pub fn new(library: AccountLibrary, request: Start, app: &LightcraftApp, files: &Files) -> Self {
        Self {
            library,
            path: request.path,
            local: u64::from(request.recover),
            confirmed: 0,
            source_revision: (app.session.catalog.revision, files.settings_revision()),
            ticket: 0,
            pending: None,
            last_status: None,
            lease: None,
            finalized: false,
        }
    }

    pub fn dirty(&self) -> bool {
        self.local != self.confirmed
    }

    fn refresh_revision(&mut self, app: &LightcraftApp, files: &Files) {
        let current = (app.session.catalog.revision, files.settings_revision());
        if self.source_revision != current {
            self.source_revision = current;
            self.local += 1;
        }
    }

    fn unfinished(app: &LightcraftApp) -> bool {
        app.gesture.is_some() || app.ui.dialog.as_ref().is_some_and(|dialog| !matches!(dialog, Dialog::Welcome { .. } | Dialog::WhatsNew))
    }

    pub fn suspended(&self) -> bool {
        self.lease.is_some()
    }

    fn status(&self, app: &LightcraftApp, importing: bool) -> Value {
        let unfinished = Self::unfinished(app);
        json!({
            "documentId":DOCUMENT_ID,
            "revision":self.local.to_string(),
            "available":true,
            "dirty":self.dirty(),
            "unfinishedEdit":unfinished,
            "capturable":!unfinished,
            "operationPending":self.pending.is_some() || importing,
        })
    }

    fn recover(&self, packet: &JsValue, app: &LightcraftApp, files: &Files) -> Result<(), String> {
        let mut reply = json!({
            "requestId":string(packet,"requestId"),
            "documentId":DOCUMENT_ID,
            "revision":integer(packet,"revision").map(|value| value.to_string()),
        });
        let bytes = if integer(packet, "revision") != Some(self.local) || Self::unfinished(app) {
            reply["message"] = json!("The LightCraft library changed before recovery capture.");
            None
        } else {
            match self.library.capture(&app.session.catalog, files) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    reply["message"] = json!(error);
                    None
                }
            }
        };
        self.library.port.publish("recovery-captured", reply, bytes)
    }

    fn capture(&mut self, kind: &str, packet: &JsValue, app: &LightcraftApp, files: &Files, importing: bool) -> Result<(), String> {
        let requested = integer(packet, "revision");
        let path = string(packet, "path");
        let mut reply = json!({
            "requestId":string(packet,"requestId"),
            "documentId":DOCUMENT_ID,
            "revision":requested.map(|value| value.to_string()),
            "reviewedRevision":requested.map(|value| value.to_string()),
            "path":path,
            "outcome":"busy",
        });
        let response = if kind == "autosave" { "autosave-result" } else { "session-save-result" };
        if self.pending.is_some()
            || importing
            || self.suspended()
            || Self::unfinished(app)
            || requested != Some(self.local)
            || kind == "autosave" && (!self.dirty() || self.path != path)
        {
            return self.library.port.publish(response, reply, None);
        }
        let Some(path) = path.filter(|path| valid_path(path)) else {
            reply["outcome"] = json!("failed");
            reply["message"] = json!("Choose an account LightCraft project path.");
            return self.library.port.publish(response, reply, None);
        };
        let bytes = match self.library.capture(&app.session.catalog, files) {
            Ok(bytes) => bytes,
            Err(error) => {
                reply["outcome"] = json!("failed");
                reply["message"] = json!(error);
                return self.library.port.publish(response, reply, None);
            }
        };
        self.ticket += 1;
        let pending = Save { ticket: self.ticket, revision: self.local, path };
        reply["ticket"] = json!(pending.ticket.to_string());
        reply["outcome"] = json!("captured");
        self.library.port.publish(response, reply, Some(bytes))?;
        self.pending = Some(pending);
        Ok(())
    }

    fn finish(&mut self, packet: &JsValue, app: &LightcraftApp, importing: bool) -> Result<(), String> {
        let Some(key) = Save::read(packet) else { return Ok(()) };
        if self.pending.as_ref() != Some(&key) {
            return Ok(());
        }
        let adopted = match string(packet, "outcome").as_deref() {
            Some("written") => {
                self.confirmed = key.revision;
                self.path = Some(key.path.clone());
                true
            }
            Some("failed") => false,
            // The host cannot correlate an uncertain write to durable account state.
            _ => return Ok(()),
        };
        self.pending = None;
        let mut response = key.packet();
        response["adopted"] = json!(adopted);
        response["projectState"] = self.status(app, importing);
        self.library.port.publish("save-settled", response, None)
    }

    fn session(&mut self, packet: &JsValue, app: &LightcraftApp, importing: bool) -> Result<(), String> {
        let operation = string(packet, "operation");
        let lease = string(packet, "lease").filter(|value| value.len() == 36);
        let matches = lease.is_some() && self.lease == lease && !self.finalized;
        let ok = match operation.as_deref() {
            Some("pause")
                if lease.is_some()
                    && !self.suspended()
                    && !importing
                    && self.pending.is_none()
                    && string(packet, "documentId").as_deref() == Some(DOCUMENT_ID)
                    && integer(packet, "revision") == Some(self.local)
                    && field(packet, "dirty").as_bool() == Some(self.dirty())
                    && field(packet, "unfinishedEdit").as_bool() == Some(Self::unfinished(app)) =>
            {
                self.library.suspend(true);
                self.lease = lease.clone();
                true
            }
            Some("release") if matches => {
                self.lease = None;
                self.library.suspend(false);
                true
            }
            Some("finalize") if matches => {
                self.finalized = true;
                true
            }
            _ => false,
        };
        self.library.port.publish(
            "session-result",
            json!({
                "requestId":string(packet,"requestId"), "operation":operation,"lease":lease,"ok":ok,
                "message":if ok { None } else { Some("The LightCraft library changed or the session lease is unavailable.") },
            }),
            None,
        )
    }

    fn operation(&self, packet: &JsValue, app: &mut LightcraftApp, importing: bool) -> Result<(), String> {
        let target = field(packet, "path");
        let valid = match &self.path {
            Some(path) => target.as_string().as_ref() == Some(path),
            None => target.is_null(),
        };
        let result = (|| {
            if !valid || self.suspended() {
                return Err("The scoped operation must target the open LightCraft library.".to_owned());
            }
            let args = string(packet, "args").filter(|value| value.len() <= 4096).ok_or("Invalid LightCraft operation arguments.")?;
            let args = crate::automation::params(&args)?;
            match string(packet, "command").as_deref() {
                Some("document.inspect") if args.as_object().is_some_and(|args| args.is_empty()) => Ok(json!({
                    "path":self.path, "revision":self.local.to_string(), "photoCount":app.session.catalog.len(),
                    "photos":app.session.catalog.photos().take(256).map(|photo| json!({
                        "id":photo.id.0, "name":photo.file_name, "rating":photo.rating,
                    })).collect::<Vec<_>>(),
                })),
                Some("photo.rate") if args.as_object().is_some_and(|args| args.len() == 2) => {
                    if importing || self.pending.is_some() || Self::unfinished(app) {
                        return Err("Finish the active LightCraft edit or file operation before rating a photo.".into());
                    }
                    let ids = args["ids"].as_array().filter(|ids| ids.len() == 1).ok_or("Choose one LightCraft photo.")?;
                    let photo = ids[0]
                        .as_u64()
                        .filter(|id| *id <= 9_007_199_254_740_991)
                        .and_then(|id| app.session.catalog.photo(PhotoId(id)))
                        .ok_or("That photo is absent from the library.")?;
                    let rating = args["rating"].as_u64().filter(|rating| *rating <= 5).ok_or("Choose a rating from zero to five.")?;
                    app.run("photo.rate", json!({"ids":[photo.id.0],"rating":rating}))
                }
                _ => Err("This LightCraft operation is unavailable.".into()),
            }
        })();
        let mut response = json!({"requestId":string(packet,"requestId"),"documentId":DOCUMENT_ID});
        match result {
            Ok(value) => response["result"] = json!(value.to_string()),
            Err(error) => response["message"] = json!(error),
        }
        self.library.port.publish("operation-result", response, None)
    }

    pub fn pump(&mut self, app: &mut LightcraftApp, files: &Files, queued_original: bool) -> Result<(), String> {
        let importing = self.library.importing() || queued_original;
        self.refresh_revision(app, files);
        for (kind, packet) in self.library.port.take_messages() {
            if kind == "session" {
                self.session(&packet, app, importing)?;
                continue;
            }
            if string(&packet, "documentId").as_deref() != Some(DOCUMENT_ID) {
                continue;
            }
            match kind.as_str() {
                "recovery-capture" => self.recover(&packet, app, files)?,
                "autosave" | "session-save" => self.capture(&kind, &packet, app, files, importing)?,
                "save-result" => self.finish(&packet, app, importing)?,
                "operation" => {
                    self.operation(&packet, app, importing)?;
                    self.refresh_revision(app, files);
                }
                _ => {}
            }
        }
        let status = self.status(app, importing);
        if self.last_status.as_ref() != Some(&status) {
            self.library.port.publish("project-state", status.clone(), None)?;
            self.last_status = Some(status);
        }
        Ok(())
    }
}
