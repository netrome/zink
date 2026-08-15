//! The zink UI (Leptos, CSR): presentation only. Every decision that isn't
//! layout — naming, ordering, threading, crypto — happens on the other side
//! of `invoke`, in the command layer and `zink-client` beneath it.

mod chat;
mod chats;
mod draft;
mod image;
mod invoke;
mod me;
mod onboarding;
mod people;
mod person;
mod picker;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use wasm_bindgen::prelude::wasm_bindgen;

use chat::ChatView;
use chats::ChatsView;
use draft::DraftChatView;
use me::MeView;
use onboarding::OnboardingView;
use people::PeopleView;
use person::PersonView;
use zink_app_dto::{AppState, Inbox, Message};

#[wasm_bindgen(start)]
pub fn start() {
    leptos::mount::mount_to_body(App);
}

/// Which screen is showing. `Chat` carries its label so the header doesn't
/// need a lookup; `Draft` is a chat with no genesis yet (project 6 S1) —
/// the picked people, nothing stored.
#[derive(Clone, PartialEq)]
enum View {
    Chats,
    Chat { id: String, label: String },
    Draft { to: Vec<String> },
    People,
    Person { petname: String },
    Me,
}

#[derive(Serialize)]
struct NoArgs {}

#[component]
fn App() -> impl IntoView {
    let view = RwSignal::new(View::Chats);
    let state = RwSignal::new(None::<AppState>);
    // True until a profile exists — drives the first-run onboarding takeover
    // (no tab bar) instead of dropping a new user into the full Me screen.
    let onboarding = RwSignal::new(false);
    let conversations = RwSignal::new(Inbox::default());
    let messages = RwSignal::new(Vec::<Message>::new());
    let status = RwSignal::new((String::new(), ""));

    let flash = move |text: String, class: &'static str| status.set((text, class));
    let ok = move |text: &str| flash(text.to_string(), "ok");
    let err = move |text: String| flash(format!("❌ {text}"), "err");

    let load_state = move || {
        spawn_local(async move {
            match invoke::invoke::<AppState>("app_state", &NoArgs {}).await {
                Ok(loaded) => {
                    // First run (no profile) → the onboarding takeover; once a
                    // profile exists, the normal app shell.
                    onboarding.set(loaded.name.is_none());
                    state.set(Some(loaded));
                }
                Err(e) => err(e),
            }
        })
    };
    let load_conversations = move || {
        spawn_local(async move {
            match invoke::invoke::<Inbox>("conversations", &NoArgs {}).await {
                Ok(inbox) => conversations.set(inbox),
                Err(e) => err(e),
            }
        })
    };
    let load_messages = move |conversation: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: &'a str,
            }
            let args = Args {
                conversation: &conversation,
            };
            match invoke::invoke::<Vec<Message>>("messages", &args).await {
                Ok(list) => messages.set(list),
                Err(e) => err(e),
            }
        })
    };

    load_state();
    load_conversations();

    // Live delivery (C4b): the Rust side emits `new-messages` per nudged
    // drain, per direct arrival (D5), and when a staged send finishes
    // delivering — which is what clears a message's "sending…" flag.
    // Always a re-render from the store; the event carries no content.
    let on_arrival = move || {
        load_conversations();
        if let View::Chat { id, .. } = view.get_untracked() {
            load_messages(id);
        }
    };
    invoke::on_event("new-messages", move |_| on_arrival());

    // …and the old poll stays as a coarse backstop (rendezvous doc §8:
    // belt & suspenders — covers a wedged subscription).
    invoke::every(60_000, move || {
        spawn_local(async move {
            if let Ok(new_count) = invoke::invoke::<usize>("refresh", &NoArgs {}).await
                && new_count > 0
            {
                on_arrival();
            }
        });
    });

    let open_chat = move |id: String, label: String| {
        load_messages(id.clone());
        view.set(View::Chat { id, label });
    };
    let start_draft = move |to: Vec<String>| view.set(View::Draft { to });
    let open_person = move |petname: String| view.set(View::Person { petname });
    // Onboarding done → drop the takeover and land on Chats.
    let finish_onboarding = move || {
        onboarding.set(false);
        load_state();
        load_conversations();
        view.set(View::Chats);
    };

    view! {
        {move || {
            if onboarding.get() {
                return view! {
                    <OnboardingView
                        reload=load_state
                        on_done=finish_onboarding
                        err=err
                    />
                }
                    .into_any();
            }
            view! {
                <div
                    id="status"
                    class=move || status.get().1
                >
                    {move || status.get().0}
                </div>
        {move || match view.get() {
            View::Chats => view! {
                <ChatsView
                    conversations=conversations
                    state=state
                    open_chat=open_chat
                    start_draft=start_draft
                />
            }
            .into_any(),
            View::Draft { to } => view! {
                <DraftChatView
                    to=to
                    open_chat=open_chat
                    back=move || view.set(View::Chats)
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Chat { id, label } => view! {
                <ChatView
                    id=id
                    label=label
                    messages=messages
                    state=state
                    reload_messages=load_messages
                    open_person=open_person
                    back=move || {
                        load_conversations();
                        view.set(View::Chats);
                    }
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::People => view! {
                <PeopleView
                    state=state
                    reload=load_state
                    open_person=open_person
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Person { petname } => view! {
                <PersonView
                    petname=petname
                    reload=load_state
                    back=move || view.set(View::People)
                    open_chat=open_chat
                    start_draft=start_draft
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Me => view! {
                <MeView state=state reload=load_state ok=ok err=err />
            }
            .into_any(),
        }}
        <nav class="tabbar">
            <button
                class:active=move || {
                    matches!(view.get(), View::Chats | View::Chat { .. } | View::Draft { .. })
                }
                on:click=move |_| {
                    load_conversations();
                    view.set(View::Chats);
                }
            >
                "Chats"
            </button>
            <button
                class:active=move || matches!(view.get(), View::People | View::Person { .. })
                on:click=move |_| {
                    load_state();
                    view.set(View::People);
                }
            >
                "People"
            </button>
            <button
                class:active=move || view.get() == View::Me
                on:click=move |_| {
                    load_state();
                    view.set(View::Me);
                }
            >
                "Me"
            </button>
        </nav>
            }
                .into_any()
        }}
    }
}

/// The best-believed avatar for a key as a data URL — `None` when nothing
/// is claimed or fetchable (render nothing; display data is best-effort).
async fn avatar_data_url(subject: &str) -> Result<Option<String>, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        subject: &'a str,
    }
    let args = Args { subject };
    let b64 = invoke::invoke::<Option<String>>("avatar", &args).await?;
    Ok(b64.map(|b64| image::data_url(&b64)))
}
