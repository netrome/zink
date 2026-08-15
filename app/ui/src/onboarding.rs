use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::QrPayload;

use crate::{image, invoke};

/// First-run onboarding (U7): a calm name → relay → your-code sequence, one
/// question per step and no tab bar, replacing the drop-into-the-full-Me-screen
/// first run. Reuses the profile / relay / avatar plumbing.
#[derive(Clone, Copy, PartialEq)]
enum OnboardStep {
    Identity,
    Relay,
    Code,
}

#[component]
pub(crate) fn OnboardingView(
    reload: impl Fn() + Copy + Send + 'static,
    on_done: impl Fn() + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let step = RwSignal::new(OnboardStep::Identity);
    let name = RwSignal::new(String::new());
    let relays = RwSignal::new(Vec::<String>::new());
    let new_relay = RwSignal::new(String::new());
    let avatar_preview = RwSignal::new(None::<String>);
    let qr = RwSignal::new(None::<QrPayload>);

    // Optional avatar (D1d): stored + claimed now; it pushes once a relay
    // exists (the app-startup re-push covers the gap).
    let pick_avatar = move |ev: leptos::ev::Event| {
        let input = event_target::<web_sys::HtmlInputElement>(&ev);
        let Some(file) = input.files().and_then(|files| files.get(0)) else {
            return;
        };
        spawn_local(async move {
            let (b64, preview) = match image::prepare_avatar(&file).await {
                Ok(prepared) => prepared,
                Err(e) => return err(e),
            };
            #[derive(Serialize)]
            struct Args<'a> {
                image: &'a str,
            }
            let args = Args { image: &b64 };
            match invoke::invoke::<usize>("set_avatar", &args).await {
                Ok(_) => avatar_preview.set(Some(preview)),
                Err(e) => err(e),
            }
        });
    };

    let add_relay = move |_| {
        let value = new_relay.get_untracked().trim().to_string();
        if value.is_empty() {
            return;
        }
        relays.update(|list| {
            if !list.contains(&value) {
                list.push(value);
            }
        });
        new_relay.set(String::new());
    };
    let remove_relay = move |value: String| {
        relays.update(|list| list.retain(|relay| relay != &value));
    };

    let finish = move |_| {
        let (name, relays) = (name.get_untracked(), relays.get_untracked());
        if name.trim().is_empty() {
            return err("choose a name first".into());
        }
        if relays.is_empty() {
            return err("add at least one relay so friends can reach you".into());
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                name: &'a str,
                relays: &'a [String],
            }
            let args = Args {
                name: &name,
                relays: &relays,
            };
            match invoke::invoke::<QrPayload>("set_profile", &args).await {
                Ok(payload) => {
                    reload();
                    qr.set(Some(payload));
                    step.set(OnboardStep::Code);
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            {move || match step.get() {
                OnboardStep::Identity => {
                    view! {
                        <h3>"welcome to zink"</h3>
                        <div class="dim">"what should people call you?"</div>
                        <input
                            placeholder="your name"
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                        />
                        <div class="pending">
                            {move || {
                                avatar_preview
                                    .get()
                                    .map(|url| view! { <img class="avatar avatar-lg" src=url /> })
                            }}
                            <label class="dim">
                                "add a photo (optional): "
                                <input type="file" accept="image/*" on:change=pick_avatar />
                            </label>
                        </div>
                        <button on:click=move |_| {
                            if name.get_untracked().trim().is_empty() {
                                err("choose a name first".into());
                            } else {
                                step.set(OnboardStep::Relay);
                            }
                        }>"continue"</button>
                    }
                        .into_any()
                }
                OnboardStep::Relay => {
                    view! {
                        <h3>"your relay"</h3>
                        <div class="dim">
                            "a relay holds your messages when you're offline. add one you run, \
                             or paste one a friend shared."
                        </div>
                        {move || {
                            relays
                                .get()
                                .into_iter()
                                .map(|relay| {
                                    let value = relay.clone();
                                    view! {
                                        <div class="row">
                                            <span class="dim" id="record-text">{relay}</span>
                                            <button
                                                class="secondary"
                                                on:click=move |_| remove_relay(value.clone())
                                            >
                                                "remove"
                                            </button>
                                        </div>
                                    }
                                })
                                .collect::<Vec<_>>()
                        }}
                        <input
                            placeholder="endpoint-id@ip:port#http://ip:port"
                            prop:value=move || new_relay.get()
                            on:input=move |ev| new_relay.set(event_target_value(&ev))
                        />
                        <button class="secondary" on:click=add_relay>
                            "add relay"
                        </button>
                        <button on:click=finish>"finish setup"</button>
                        <button
                            class="secondary"
                            on:click=move |_| step.set(OnboardStep::Identity)
                        >
                            "back"
                        </button>
                    }
                        .into_any()
                }
                OnboardStep::Code => {
                    view! {
                        <h3>"you're all set"</h3>
                        <div class="dim">
                            "this is your code — let a friend scan it so they can reach you"
                        </div>
                        {move || {
                            qr.get()
                                .map(|record| {
                                    view! {
                                        <div id="qr" inner_html=record.svg></div>
                                        <div id="record-text">{record.text}</div>
                                    }
                                })
                        }}
                        <button on:click=move |_| on_done()>"start chatting"</button>
                        <div class="dim">"add friends anytime from the People tab"</div>
                    }
                        .into_any()
                }
            }}
        </main>
    }
}
