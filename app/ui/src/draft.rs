use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{Conversation, OutgoingImage};

use crate::chat::Composer;
use crate::invoke;

/// A chat that does not exist yet (project 6 S1): people picked, no genesis.
/// Renders honestly — empty history, a line naming what the first send does
/// — and lists the existing conversations with exactly these people
/// (discovery, never auto-routing: several conversations per set is a
/// feature). The first send stages the genesis; success promotes this view
/// to the real chat.
#[component]
pub(crate) fn DraftChatView(
    to: Vec<String>,
    open_chat: impl Fn(String, String) + Copy + Send + 'static,
    back: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let header = to.join(", ");
    let title = StoredValue::new(header.clone());
    let to = StoredValue::new(to);
    let draft = RwSignal::new(String::new());
    let attachment = RwSignal::new(None::<(OutgoingImage, String)>);

    // Existing conversations with exactly this set — the discovery list.
    let existing = RwSignal::new(Vec::<Conversation>::new());
    spawn_local(async move {
        #[derive(Serialize)]
        struct Args {
            to: Vec<String>,
        }
        let args = Args { to: to.get_value() };
        match invoke::invoke::<Vec<Conversation>>("conversations_with", &args).await {
            Ok(list) => existing.set(list),
            Err(e) => err(e),
        }
    });

    let send = move || {
        let body = draft.get_untracked();
        let image = attachment.get_untracked().map(|(image, _)| image);
        if body.trim().is_empty() && image.is_none() {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: Option<&'a str>,
                to: Option<Vec<String>>,
                text: &'a str,
                image: Option<OutgoingImage>,
            }
            let args = Args {
                conversation: None,
                to: Some(to.get_value()),
                text: &body,
                image,
            };
            match invoke::invoke::<String>("send_message", &args).await {
                Ok(conversation) => {
                    // Stored, not yet delivered — the message's own
                    // "sending…" marker is the truth from here.
                    ok("sending…");
                    open_chat(conversation, title.get_value());
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            <div class="picks">
                <button class="secondary" on:click=move |_| back()>
                    "‹ chats"
                </button>
            </div>
            <h3>{header}</h3>
            <div class="messages">
                {move || {
                    let list = existing.get();
                    (!list.is_empty())
                        .then(|| {
                            view! {
                                <div class="panel">
                                    <div class="dim">
                                        {format!(
                                            "you already have {} conversation(s) with these people — or send below to start another",
                                            list.len(),
                                        )}
                                    </div>
                                    {list
                                        .into_iter()
                                        .map(|conversation| {
                                            let (id, label) = (
                                                conversation.id.clone(),
                                                conversation.label.clone(),
                                            );
                                            view! {
                                                <div
                                                    class="row"
                                                    on:click=move |_| open_chat(id.clone(), label.clone())
                                                >
                                                    <b>{conversation.label}</b>
                                                    <span class="dim">
                                                        {format!("{} message(s)", conversation.message_count)}
                                                    </span>
                                                </div>
                                            }
                                        })
                                        .collect::<Vec<_>>()}
                                </div>
                            }
                        })
                }}
                <div class="dim">
                    {format!(
                        "your first message starts a new conversation with {}",
                        title.get_value(),
                    )}
                </div>
            </div>
            <div class="compose">
                <Composer draft=draft attachment=attachment send=send err=err />
            </div>
        </main>
    }
}
