use crate::{CommandResult, Id};

use atoman::Result;
use pearce::{Bytes, Callback};
use rigging::widgets::Confirmation;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum Dialog {
    Script {
        id: String,
        code: String,
        #[serde(default)]
        use_sudo: bool,
    },
    Confirm {
        id: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Confirmation>,
    },
    Prompt {
        id: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        multiline: Option<bool>,
    },
    Secret {
        id: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Select {
        id: String,
        prompt: String,
        items: Vec<String>,
    },
}

impl Dialog {
    /// Returns dialog event ID.
    pub fn get_id(&self) -> &str {
        match self {
            Self::Confirm { id, .. } => id,
            Self::Prompt { id, .. } => id,
            Self::Secret { id, .. } => id,
            Self::Select { id, .. } => id,
            Self::Script { id, .. } => id,
        }
    }

    /// Universal method for sending any dialogue and waiting for the result.
    pub async fn dispatch<T>(self, tx: &mpsc::Sender<Bytes>, timeout: Duration) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let id = self.get_id().to_string();
        let mut callback = Callback::register(&id);
        tx.send(Event::Dialog(self).into()).await?;

        use atoman::error::Error;

        let output = tokio::select! {
            _ = tx.closed() => return Err(Error::Custom(format!("Channel is already closed.")).into()),
            res = callback.recv::<T>(timeout) => {
                res.map_err(|e| Error::Custom(format!("Dialog action failed: {e}")))?
            }
        };

        Ok(output)
    }

    /// Sends script for execution in CLI (with CommandResult waiting).
    pub async fn script(
        tx: &mpsc::Sender<Bytes>,
        code: impl Into<String>,
        timeout: Duration,
    ) -> Result<Option<CommandResult>> {
        let id = Id::new().to_string();
        let dialog = Self::Script {
            id,
            code: code.into(),
            use_sudo: false,
        };

        dialog.dispatch(tx, timeout).await
    }

    /// Sends script for execution in CLI (with CommandResult waiting).
    pub async fn sudo_script(
        tx: &mpsc::Sender<Bytes>,
        code: impl Into<String>,
        timeout: Duration,
    ) -> Result<Option<CommandResult>> {
        let id = Id::new().to_string();
        let dialog = Self::Script {
            id,
            code: code.into(),
            use_sudo: true,
        };

        dialog.dispatch(tx, timeout).await
    }

    /// Sends confirmation request [y/n].
    pub async fn confirm(
        tx: &mpsc::Sender<Bytes>,
        prompt: impl Into<String>,
        default: Option<Confirmation>,
        timeout: Duration,
    ) -> Result<Option<Confirmation>> {
        let id = Id::new().to_string();
        let dialog = Self::Confirm {
            id,
            prompt: prompt.into(),
            default,
        };

        dialog.dispatch(tx, timeout).await
    }

    /// Sends input prompt.
    pub async fn prompt(
        tx: &mpsc::Sender<Bytes>,
        prompt: impl Into<String>,
        placeholder: Option<String>,
        default: Option<String>,
        timeout: Duration,
    ) -> Result<Option<String>> {
        let id = Id::new().to_string();
        let dialog = Self::Prompt {
            id,
            prompt: prompt.into(),
            placeholder,
            default,
            multiline: Some(false),
        };

        dialog.dispatch(tx, timeout).await
    }

    /// Sends multiline input prompt.
    pub async fn multiline_prompt(
        tx: &mpsc::Sender<Bytes>,
        prompt: impl Into<String>,
        placeholder: Option<String>,
        default: Option<String>,
        timeout: Duration,
    ) -> Result<Option<String>> {
        let id = Id::new().to_string();
        let dialog = Self::Prompt {
            id,
            prompt: prompt.into(),
            placeholder,
            default,
            multiline: Some(true),
        };

        dialog.dispatch(tx, timeout).await
    }

    /// Sends secret input prompt (passwords, tokens, etc.).
    pub async fn secret(
        tx: &mpsc::Sender<Bytes>,
        prompt: impl Into<String>,
        placeholder: Option<String>,
        timeout: Duration,
    ) -> Result<Option<String>> {
        let id = Id::new().to_string();
        let dialog = Self::Secret {
            id,
            prompt: prompt.into(),
            placeholder,
        };

        dialog.dispatch(tx, timeout).await
    }

    /// Sends drop‑down menu for selecting from a list.
    pub async fn select(
        tx: &mpsc::Sender<Bytes>,
        prompt: impl Into<String>,
        items: Vec<String>,
        timeout: Duration,
    ) -> Result<Option<Option<usize>>> {
        let id = Id::new().to_string();
        let dialog = Self::Select {
            id,
            prompt: prompt.into(),
            items,
        };

        dialog.dispatch(tx, timeout).await
    }
}

/// Assistant stream event
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Event {
    Thinking(String),
    Answer(String),
    Error(String),
    Dialog(Dialog),
    Finish,
}

impl From<Dialog> for Event {
    fn from(dialog: Dialog) -> Self {
        Event::Dialog(dialog)
    }
}

impl From<Dialog> for Bytes {
    fn from(dialog: Dialog) -> Self {
        Event::from(dialog).into()
    }
}

impl From<Event> for String {
    fn from(event: Event) -> Self {
        serde_json::to_string(&event).expect("Event serialization should not fail")
    }
}

impl From<Event> for Bytes {
    fn from(event: Event) -> Self {
        String::from(event).into()
    }
}
