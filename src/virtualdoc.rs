//! A server's own documents — what a definition can point at that isn't a file: jdtls's library and JDK
//! classes as `jdt://contents/…`. Its pack's `[server.<name>] virtual` says the scheme, the request that
//! returns one's text and the language it's in (`lsp::Virtual`); the text goes into a read-only buffer
//! (`Document::virtual_uri`), found again by its URI the next time.

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::lsp::ClientId;

impl Editor {
    /// Go to `uri` (one of server `cid`'s own documents) at `pos` — its buffer if open, else asked for.
    pub fn open_server_doc(&mut self, cid: ClientId, uri: String, pos: Value) {
        if let Some(i) = self.docs.iter().position(|d| d.virtual_uri.as_deref() == Some(uri.as_str())) {
            self.current = i;
            return self.jump_in_current(cid, &pos);
        }
        let Some(fetch) =
            self.client(cid).and_then(|c| c.traits.virtual_docs.as_ref()).map(|v| v.fetch.clone())
        else {
            return;
        };
        let name = self.client(cid).map(|c| c.name.clone()).unwrap_or_default();
        let kind = crate::lsp_editor::Kind::Virtual { uri: uri.clone(), pos };
        if self.lsp_send(cid, kind, &fetch, json!({ "uri": uri })).is_none() {
            self.set_status(format!("{name} is starting…"));
        }
    }

    fn jump_in_current(&mut self, cid: ClientId, pos: &Value) {
        let enc = self.encoding_of(cid);
        let doc = self.doc_mut();
        if let Some(b) = crate::lsp::from_position(&doc.text, pos, enc) {
            doc.set_selection(crate::selection::Selection::point(b));
        }
    }

    /// The answer for `uri`: its text into a read-only buffer in the pack's language, then to `pos`.
    pub fn server_doc_reply(
        &mut self,
        cid: ClientId,
        uri: String,
        pos: &Value,
        error: Option<&Value>,
        result: &Value,
    ) {
        let Some(v) = self.client(cid).and_then(|c| c.traits.virtual_docs.as_ref()) else { return };
        let (title, language) = (v.title(&uri), v.language.clone());
        if let Some(e) = error {
            return self.set_error(format!("{title}: {}", e["message"].as_str().unwrap_or("failed")));
        }
        let Some(text) = result.as_str().filter(|t| !t.trim().is_empty()) else {
            return self.set_status(format!("No source for {title}"));
        };
        let doc_id = self.alloc_id();
        let mut doc = crate::document::Document::from_str(doc_id, text);
        doc.title = Some(title);
        doc.throwaway = true;
        doc.virtual_uri = Some(uri);
        self.docs.push(doc);
        self.current = self.docs.len() - 1;
        if let Some(spec) = crate::syntax::spec(&language) {
            self.load_language(doc_id, spec);
        }
        self.jump_in_current(cid, pos);
    }
}
