use std::collections::BTreeSet;

use leptos::prelude::*;
use zink_app_dto::{AppState, Conversation, Inbox, Key};

use crate::picker::PeoplePicker;

/// The new-chat picker's in-progress selection (S4, U8): App-lifetime
/// signals, so a tab bounce (which remounts the view) can't destroy picks.
#[derive(Clone, Copy)]
pub(crate) struct NewChatPicker {
    pub picking: RwSignal<bool>,
    pub selected: RwSignal<BTreeSet<String>>,
}

impl Default for NewChatPicker {
    fn default() -> Self {
        Self {
            picking: RwSignal::new(false),
            selected: RwSignal::new(BTreeSet::new()),
        }
    }
}

/// One conversation row (S5): label + relative time on the first line, a
/// one-line preview beneath. Shared by the chats list, the draft's
/// discovery list, and the person view's conversations.
#[component]
pub(crate) fn ConversationRow(
    conversation: Conversation,
    open: impl Fn(String, String) + Copy + Send + 'static,
) -> impl IntoView {
    let (id, label) = (conversation.id.clone(), conversation.label.clone());
    view! {
        <div class="row" on:click=move |_| open(id.clone(), label.clone())>
            <div class="row-top">
                <b>{conversation.label}</b>
                <span class="row-meta">
                    {(conversation.unread > 0)
                        .then(|| view! { <span class="badge">{conversation.unread}</span> })}
                    <span class="dim">{when(conversation.last_timestamp_ms)}</span>
                </span>
            </div>
            {(!conversation.snippet.is_empty())
                .then(|| view! { <div class="dim snippet">{conversation.snippet}</div> })}
        </div>
    }
}

/// Row-sized relative time: hh:mm today, else the day ("yesterday",
/// "aug 12") — the sender's wall-clock hint, display only.
fn when(timestamp_ms: u64) -> String {
    if crate::chat::day_of(timestamp_ms) == crate::chat::day_of(js_sys::Date::now() as u64) {
        crate::chat::time_of(timestamp_ms)
    } else {
        crate::chat::day_label(timestamp_ms)
    }
}

/// Conversation list. Starting a new chat is a deliberate "+" action: pick
/// people in the shared picker, then land in a draft chat whose first send
/// is the genesis (project 6 §7 — a new chat is always a new conversation).
/// No permanent form, no refresh button — live delivery + the backstop poll
/// keep the list current.
#[component]
pub(crate) fn ChatsView(
    conversations: RwSignal<Inbox>,
    state: RwSignal<Option<AppState>>,
    picker: NewChatPicker,
    open_chat: impl Fn(String, String) + Copy + Send + 'static,
    start_draft: impl Fn(Vec<String>) + Copy + Send + 'static,
    open_key: impl Fn(Key) + Copy + Send + 'static,
) -> impl IntoView {
    // Whether the "new chat" picker is open (vs the plain list), plus the
    // picks — App-owned, surviving a tab bounce.
    let NewChatPicker { picking, selected } = picker;
    let contacts =
        Signal::derive(move || state.get().map(|state| state.contacts).unwrap_or_default());

    let close_picker = move || {
        picking.set(false);
        selected.update(|selected| selected.clear());
    };
    let next = move |_| {
        let picks: Vec<String> = selected.get_untracked().into_iter().collect();
        if picks.is_empty() {
            return;
        }
        close_picker();
        start_draft(picks);
    };

    view! {
        <main>
            {move || {
                if picking.get() {
                    // The "new chat" picker: pick people, then the draft chat.
                    view! {
                        <h3>"new chat"</h3>
                        <PeoplePicker contacts=contacts selected=selected />
                        <button
                            disabled=move || selected.with(|selected| selected.is_empty())
                            on:click=next
                        >
                            "next"
                        </button>
                        <button class="secondary" on:click=move |_| close_picker()>
                            "cancel"
                        </button>
                    }
                        .into_any()
                } else {
                    // The plain list + the deliberate "+" to start a chat.
                    view! {
                        <button on:click=move |_| picking.set(true)>"+ new chat"</button>
                        {move || {
                            let inbox = conversations.get();
                            let row = |conversation: Conversation| {
                                view! {
                                    <ConversationRow
                                        conversation=conversation
                                        open=open_chat
                                    />
                                }
                            };
                            if inbox.conversations.is_empty() && inbox.requests.is_empty() {
                                view! {
                                    <div class="dim">
                                        "no conversations yet — tap + new chat to start one"
                                    </div>
                                }
                                    .into_any()
                            } else {
                                let main = inbox
                                    .conversations
                                    .into_iter()
                                    .map(row)
                                    .collect::<Vec<_>>();
                                // Message requests (groups.md §6): nobody you
                                // know has written here yet. Kept out of the
                                // main list, never hidden — and phrased as
                                // "not yet", because one message from a
                                // contact promotes the whole conversation.
                                let (requests, dropped) = (inbox.requests, inbox.dropped);
                                let pending = (!requests.is_empty())
                                    .then(|| {
                                        let rows = requests
                                            .into_iter()
                                            .map(|conversation| {
                                                // Preview the sender before
                                                // opening the chat (S3): the
                                                // person page, no query fired.
                                                let stranger = conversation
                                                    .stranger_key
                                                    .clone();
                                                view! {
                                                    {row(conversation)}
                                                    {stranger
                                                        .map(|key| {
                                                            view! {
                                                                <button
                                                                    class="secondary"
                                                                    on:click=move |_| open_key(key.clone())
                                                                >
                                                                    "who is this?"
                                                                </button>
                                                            }
                                                        })}
                                                }
                                            })
                                            .collect::<Vec<_>>();
                                        view! {
                                            <div class="dim section">
                                                "message requests — nobody you know has written here yet"
                                            </div>
                                            {rows}
                                            {(dropped > 0)
                                                .then(|| {
                                                    view! {
                                                        <div class="dim">
                                                            {format!("+ {dropped} more not shown")}
                                                        </div>
                                                    }
                                                })}
                                        }
                                    });
                                view! {
                                    {main}
                                    {pending}
                                }
                                    .into_any()
                            }
                        }}
                    }
                        .into_any()
                }
            }}
        </main>
    }
}
