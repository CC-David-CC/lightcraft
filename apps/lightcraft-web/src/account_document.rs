//! Versioned account snapshots of the editor's native catalog.

use lightcraft_ui_egui::LightcraftApp;
use serde_json::{Value, json};
use wasm_bindgen::JsValue;

use crate::account_library::AccountLibrary;
use crate::account_port::{Start, string};
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
        app.gesture.is_some() || app.ui.dialog.is_some()
    }

    fn status(&self, app: &LightcraftApp) -> Value {
        let unfinished = Self::unfinished(app);
        json!({
            "documentId":DOCUMENT_ID,
            "revision":self.local.to_string(),
            "available":true,
            "dirty":self.dirty(),
            "unfinishedEdit":unfinished,
            "capturable":!unfinished,
            "operationPending":self.pending.is_some(),
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

    fn capture(&mut self, kind: &str, packet: &JsValue, app: &LightcraftApp, files: &Files) -> Result<(), String> {
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

    fn finish(&mut self, packet: &JsValue, app: &LightcraftApp) -> Result<(), String> {
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
        response["projectState"] = self.status(app);
        self.library.port.publish("save-settled", response, None)
    }

    pub fn pump(&mut self, app: &LightcraftApp, files: &Files) -> Result<(), String> {
        self.refresh_revision(app, files);
        for (kind, packet) in self.library.port.take_messages() {
            if string(&packet, "documentId").as_deref() != Some(DOCUMENT_ID) {
                continue;
            }
            match kind.as_str() {
                "recovery-capture" => self.recover(&packet, app, files)?,
                "autosave" | "session-save" => self.capture(&kind, &packet, app, files)?,
                "save-result" => self.finish(&packet, app)?,
                _ => {}
            }
        }
        let status = self.status(app);
        if self.last_status.as_ref() != Some(&status) {
            self.library.port.publish("project-state", status.clone(), None)?;
            self.last_status = Some(status);
        }
        Ok(())
    }
}
