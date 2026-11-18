//! The same-origin account transport. Only the owning Desktop frame may answer requests.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{MessageEvent, Window};

const PREFIX: &str = "lightcraft:account-";
const MAX_BYTES: u32 = 128 * 1024 * 1024;
const REQUEST_TIMEOUT_MS: i32 = 120_000;

type Request = (String, String);

struct Transport {
    parent: Window,
    origin: String,
    generation: String,
    sequence: Cell<u64>,
    pending: RefCell<BTreeMap<Request, Function>>,
}

struct Listener {
    window: Window,
    callback: Closure<dyn Fn(MessageEvent)>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.window.remove_event_listener_with_callback("message", self.callback.as_ref().unchecked_ref());
    }
}

#[derive(Clone)]
pub(crate) struct AccountPort {
    transport: Rc<Transport>,
    listener: Rc<Listener>,
}

pub(crate) struct Start {
    pub request_id: String,
    pub path: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub recover: bool,
}

fn field(packet: &JsValue, name: &str) -> JsValue {
    Reflect::get(packet, &name.into()).unwrap_or(JsValue::UNDEFINED)
}

fn string(packet: &JsValue, name: &str) -> Option<String> {
    field(packet, name).as_string()
}

fn read_bytes(value: JsValue) -> Result<Vec<u8>, String> {
    let buffer = value.dyn_into::<ArrayBuffer>().map_err(|_| "The account did not send file bytes.".to_owned())?;
    if buffer.byte_length() == 0 || buffer.byte_length() > MAX_BYTES {
        return Err("The account file size is invalid.".into());
    }
    Ok(Uint8Array::new(&buffer).to_vec())
}

fn hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl AccountPort {
    pub fn connect() -> Result<Self, String> {
        let no_parent = || "An account library requires its same-origin WebOS window.".to_owned();
        let window = web_sys::window().ok_or_else(no_parent)?;
        let parent = window.parent().ok().flatten().ok_or_else(no_parent)?;
        let origin = window.origin();
        if Object::is(&parent, &window)
            || !crate::host::origin_is_usable(&origin)
            || parent.location().origin().ok().as_deref() != Some(origin.as_str())
        {
            return Err(no_parent());
        }
        let mut random = [0; 16];
        window
            .crypto()
            .and_then(|crypto| crypto.get_random_values_with_u8_array(&mut random))
            .map_err(|_| "Could not create an account window identity.")?;
        let generation = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let transport = Rc::new(Transport { parent, origin, generation, sequence: Cell::new(0), pending: RefCell::default() });
        let owner = Rc::downgrade(&transport);
        let callback = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
            if let Some(owner) = owner.upgrade() {
                owner.receive(event);
            }
        });
        window
            .add_event_listener_with_callback("message", callback.as_ref().unchecked_ref())
            .map_err(|_| "Could not listen to the WebOS account window.")?;
        Ok(Self { transport, listener: Rc::new(Listener { window, callback }) })
    }

    fn post(&self, kind: &str, fields: &[(&str, JsValue)], transfer: Option<&ArrayBuffer>) -> Result<(), String> {
        let packet = Object::new();
        for (name, value) in
            [("type", format!("{PREFIX}{kind}").into()), ("generation", self.transport.generation.as_str().into())].iter().chain(fields)
        {
            Reflect::set(&packet, &(*name).into(), value).map_err(|_| "Could not make the account request.")?;
        }
        let parent = &self.transport.parent;
        let origin = &self.transport.origin;
        let sent = match transfer {
            Some(buffer) => parent.post_message_with_transfer(&packet, origin, &Array::of1(buffer)),
            None => parent.post_message(&packet, origin),
        };
        sent.map_err(|_| "Could not reach the account window.".into())
    }

    fn wait(&self, key: &Request) -> Result<(JsFuture, i32, Closure<dyn FnMut()>), String> {
        let mut reject = None;
        let promise = Promise::new(&mut |resolve, failed| {
            self.transport.pending.borrow_mut().insert(key.clone(), resolve);
            reject = Some(failed);
        });
        let owner = Rc::downgrade(&self.transport);
        let request = key.clone();
        let timeout = reject.ok_or("Could not schedule an account request.")?;
        let callback = Closure::once(move || {
            if owner.upgrade().is_some_and(|owner| owner.pending.borrow_mut().remove(&request).is_some()) {
                let _ = timeout.call1(&JsValue::UNDEFINED, &"The account window did not answer.".into());
            }
        });
        let timer = self.listener.window.set_timeout_with_callback_and_timeout_and_arguments_0(callback.as_ref().unchecked_ref(), REQUEST_TIMEOUT_MS);
        match timer {
            Ok(timer) => Ok((JsFuture::from(promise), timer, callback)),
            Err(_) => {
                self.transport.pending.borrow_mut().remove(key);
                Err("Could not start an account request.".into())
            }
        }
    }

    async fn exchange(&self, key: Request, outgoing: &str, fields: &[(&str, JsValue)], transfer: Option<&ArrayBuffer>) -> Result<JsValue, String> {
        let (reply, timer, callback) = self.wait(&key)?;
        let response = match self.post(outgoing, fields, transfer) {
            Ok(()) => reply.await.map_err(|error| error.as_string().unwrap_or_else(|| "The account request failed.".into())),
            Err(error) => Err(error),
        };
        self.listener.window.clear_timeout_with_handle(timer);
        self.transport.pending.borrow_mut().remove(&key);
        drop(callback);
        response
    }

    pub async fn start(&self) -> Result<Start, String> {
        let packet = self.exchange(("start".into(), String::new()), "ready", &[], None).await?;
        let request_id = string(&packet, "requestId").ok_or("Missing account Open request identity.")?;
        let parsed = (|| {
            let path = field(&packet, "path");
            let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid account project path.")?) };
            let recover = field(&packet, "recover").as_bool().ok_or("Invalid account recovery request.")?;
            let content = field(&packet, "bytes");
            let bytes = if content.is_null() { None } else { Some(read_bytes(content)?) };
            if !field(&packet, "importName").is_null() || bytes.is_none() && (path.is_some() || recover) {
                return Err("Invalid LightCraft account library request.".to_owned());
            }
            Ok(Start { request_id: request_id.clone(), path, bytes, recover })
        })();
        if let Err(error) = &parsed {
            self.open_failed(&request_id, error);
        }
        parsed
    }

    pub fn open_failed(&self, request_id: &str, error: &str) {
        let _ = self.post("started", &[("requestId", request_id.into()), ("ok", false.into()), ("message", error.into())], None);
    }

    pub fn opened(&self, request: &Start) -> Result<(), String> {
        self.post(
            "started",
            &[
                ("requestId", request.request_id.as_str().into()),
                ("ok", true.into()),
                ("path", request.path.as_deref().map_or(JsValue::NULL, JsValue::from_str)),
                ("documentId", "1".into()),
                ("revision", if request.recover { "1" } else { "0" }.into()),
            ],
            None,
        )
    }

    pub fn startup_failed(&self, error: &str) {
        let _ = self.post("failed", &[("message", error.into())], None);
    }

    async fn original(&self, operation: &str, field: (&str, JsValue), transfer: Option<&ArrayBuffer>) -> Result<JsValue, String> {
        let id = self.transport.sequence.get().checked_add(1).ok_or("Too many original requests.")?;
        self.transport.sequence.set(id);
        let id = id.to_string();
        let result = self.exchange((operation.into(), id.clone()), operation, &[("requestId", id.into()), field], transfer).await?;
        if self::field(&result, "ok").as_bool() != Some(true) {
            return Err(string(&result, "message").unwrap_or_else(|| "Could not transfer the account original.".into()));
        }
        Ok(result)
    }

    pub async fn read_original(&self, digest: &str) -> Result<Vec<u8>, String> {
        if !hex_digest(digest) {
            return Err("Invalid LightCraft original digest.".into());
        }
        let reply = self.original("original-read", ("digest", digest.into()), None).await?;
        read_bytes(field(&reply, "bytes"))
    }

    pub async fn store_original(&self, bytes: &[u8]) -> Result<String, String> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES as usize {
            return Err("Choose an original no larger than 128 MiB.".into());
        }
        let buffer = Uint8Array::from(bytes).buffer();
        let reply = self.original("original-store", ("bytes", buffer.clone().into()), Some(&buffer)).await?;
        string(&reply, "digest").filter(|digest| hex_digest(digest)).ok_or_else(|| "The account did not confirm the original.".into())
    }
}

impl Transport {
    fn receive(&self, event: MessageEvent) {
        if event.origin() != self.origin || !event.source().is_some_and(|source| Object::is(&source, &self.parent)) {
            return;
        }
        let packet = event.data();
        if !packet.is_object() || string(&packet, "generation").as_deref() != Some(self.generation.as_str()) {
            return;
        }
        let request = match string(&packet, "type").as_deref().and_then(|kind| kind.strip_prefix(PREFIX)) {
            Some("start") => ("start".into(), String::new()),
            Some("original-result") => {
                let (Some(operation), Some(request)) = (string(&packet, "operation"), string(&packet, "requestId")) else { return };
                (operation, request)
            }
            _ => return,
        };
        let resolve = self.pending.borrow_mut().remove(&request);
        if let Some(resolve) = resolve {
            let _ = resolve.call1(&JsValue::UNDEFINED, &packet);
        }
    }
}
