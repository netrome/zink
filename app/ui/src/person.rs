use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{PersonDetail, WhoIsReport};

use crate::{avatar_data_url, image, invoke};

/// The person-detail lens (U4, design/ui-design-system.md §1): one contact rendered as
/// three separated belief layers — my lens (petname + avatar + the keys I've
/// grouped), their self-claim, and my friends' lens (vouched names only,
/// never a friend's private petname). Trust actions (vouch / repudiate) and a
/// who-is freshness pull live here, in context. All read-time; nothing here
/// assumes one key per person.
#[component]
pub(crate) fn PersonView(
    petname: String,
    reload: impl Fn() + Copy + Send + 'static,
    back: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let petname = StoredValue::new(petname);
    let detail = RwSignal::new(None::<PersonDetail>);
    let avatar = RwSignal::new(None::<String>);
    // The editable petname (my lens) — prefilled from the loaded detail.
    let rename_to = RwSignal::new(String::new());
    // Repudiation is armed-then-confirmed (two taps) — it publishes.
    let armed = RwSignal::new(false);

    let load_detail = move || {
        let name = petname.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
            }
            let args = Args { petname: &name };
            match invoke::invoke::<PersonDetail>("person_detail", &args).await {
                Ok(loaded) => {
                    let key = loaded.avatar_key.clone();
                    rename_to.set(loaded.petname.clone());
                    detail.set(Some(loaded));
                    if !key.is_empty()
                        && let Ok(url) = avatar_data_url(&key).await
                    {
                        avatar.set(url);
                    }
                }
                Err(e) => err(e),
            }
        });
    };

    // Rename — set my petname for them (my lens). Local only; sharing that
    // name with friends is the separate `vouch` below.
    let do_rename = move || {
        let current = petname.get_value();
        let new = rename_to.get_untracked();
        if new.trim().is_empty() || new == current {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                current: &'a str,
                new: &'a str,
            }
            let args = Args {
                current: &current,
                new: &new,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("rename_contact", &args).await {
                Ok(_) => {
                    // The view now tracks the new name (person_detail is
                    // keyed by petname).
                    petname.set_value(new.clone());
                    reload();
                    load_detail();
                    ok(&format!("renamed to {new}"));
                }
                Err(e) => err(e),
            }
        });
    };
    load_detail();

    let toggle_vouch = move || {
        let Some(current) = detail.get_untracked() else {
            return;
        };
        let (name, vouched) = (current.petname, current.vouched);
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
            }
            let args = Args { petname: &name };
            let command = if vouched { "unvouch" } else { "vouch" };
            match invoke::invoke::<serde::de::IgnoredAny>(command, &args).await {
                Ok(_) => {
                    reload();
                    load_detail();
                    ok(if vouched {
                        "no longer vouching for them"
                    } else {
                        "vouching — shares the name you call them with anyone \
                         who asks you about them"
                    });
                }
                Err(e) => err(e),
            }
        });
    };

    // Re-ask the network (D1c): fresh answers land in the learned store and
    // sharpen resolution on their own — reload the detail to show them.
    let refresh = move || {
        let Some(subject) = detail.get_untracked().map(|person| person.avatar_key) else {
            return;
        };
        if subject.is_empty() {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &subject };
            match invoke::invoke::<WhoIsReport>("who_is", &args).await {
                Ok(report) => {
                    ok(&format!(
                        "{} answer(s) (asked {}, {} unreachable) — fresh records apply automatically",
                        report.answers, report.asked, report.unreachable
                    ));
                    load_detail();
                }
                Err(e) => err(e),
            }
        });
    };

    let repudiate = move || {
        let Some(key) = detail.get_untracked().map(|person| person.avatar_key) else {
            return;
        };
        if key.is_empty() {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("repudiate_key", &args).await {
                Ok(_) => {
                    armed.set(false);
                    reload();
                    load_detail();
                    ok("marked compromised — published in your record; contacts \
                        learn it from their next pull");
                }
                Err(e) => err(e),
            }
        });
    };

    // Manual relay override (R5, my lens like the petname): the escape
    // hatch when their record is stale and a rescan isn't at hand. Wins
    // resolution until cleared — or until a confirmed rescan supersedes it.
    let override_input = RwSignal::new(String::new());
    let set_override = move |relays: Vec<String>| {
        let name = petname.get_value();
        let cleared = relays.is_empty();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
                relays: &'a [String],
            }
            let args = Args {
                petname: &name,
                relays: &relays,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("set_relay_override", &args).await {
                Ok(_) => {
                    override_input.set(String::new());
                    load_detail();
                    ok(if cleared {
                        "override cleared — their record is back in use"
                    } else {
                        "override set — your relays win until you clear them or rescan"
                    });
                }
                Err(e) => err(e),
            }
        });
    };

    // A local photo for them (U6, my lens): a photo *I* chose, stored on
    // this device only — never published. Overrides their self-claim
    // everywhere their avatar shows.
    let set_photo = move |ev: leptos::ev::Event, key: String| {
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
                key: &'a str,
                image: &'a str,
            }
            let args = Args {
                key: &key,
                image: &b64,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("set_local_avatar", &args).await {
                Ok(_) => {
                    avatar.set(Some(preview));
                    reload();
                    load_detail();
                    ok("photo set — only you see it");
                }
                Err(e) => err(e),
            }
        });
    };
    let clear_photo = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("clear_local_avatar", &args).await {
                Ok(_) => {
                    // Fall back to their self-claimed avatar (or none).
                    avatar.set(avatar_data_url(&key).await.ok().flatten());
                    reload();
                    load_detail();
                    ok("using their photo again");
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            <button class="secondary" on:click=move |_| back()>
                "‹ people"
            </button>
            {move || {
                detail
                    .get()
                    .map(|person| {
                        let friends = person.friends.clone();
                        let keys = person.keys.clone();
                        let disavowals = person.disavowals.clone();
                        let has_key = !person.avatar_key.is_empty();
                        let photo_key = person.avatar_key.clone();
                        let has_local = person.has_local_avatar;
                        view! {
                            // My lens: avatar + the petname I call them.
                            <div class="pending">
                                {move || {
                                    avatar
                                        .get()
                                        .map(|url| view! { <img class="avatar avatar-lg" src=url /> })
                                }}
                                <h3>{person.petname.clone()}</h3>
                            </div>
                            // My lens: the name I call them (editable, local).
                            <div class="dim">"your name for them"</div>
                            <input
                                prop:value=move || rename_to.get()
                                on:input=move |ev| rename_to.set(event_target_value(&ev))
                            />
                            <button class="secondary" on:click=move |_| do_rename()>
                                "rename"
                            </button>
                            // A local photo for them (U6) — only you see it.
                            {(!photo_key.is_empty())
                                .then(|| {
                                    let pick_key = photo_key.clone();
                                    let clear_key = photo_key.clone();
                                    view! {
                                        <label class="dim">
                                            "your photo for them: "
                                            <input
                                                type="file"
                                                accept="image/*"
                                                on:change=move |ev| set_photo(ev, pick_key.clone())
                                            />
                                        </label>
                                        {has_local
                                            .then(|| {
                                                view! {
                                                    <button
                                                        class="secondary"
                                                        on:click=move |_| clear_photo(clear_key.clone())
                                                    >
                                                        "use their photo instead"
                                                    </button>
                                                }
                                            })}
                                    }
                                })}
                            // Disavowal warnings — context at the moment of decision.
                            {disavowals
                                .into_iter()
                                .map(|line| {
                                    view! {
                                        <div class="row">
                                            <span class="dim">{line}</span>
                                        </div>
                                    }
                                })
                                .collect::<Vec<_>>()}
                            // Their self-claim.
                            <div class="dim">"they call themselves"</div>
                            <div class="row">
                                <b>{person.self_name.clone().unwrap_or_else(|| "—".to_string())}</b>
                            </div>
                            // Friends' lens: vouched names only — never a
                            // friend's private petname (who-is-this.md §6).
                            <div class="dim">"how your friends see them"</div>
                            {if friends.is_empty() {
                                view! {
                                    <div class="row">
                                        <span class="dim">"no friend has vouched a name for them yet"</span>
                                    </div>
                                }
                                    .into_any()
                            } else {
                                friends
                                    .into_iter()
                                    .map(|friend| {
                                        view! {
                                            <div class="row">
                                                <b>{friend.name}</b>
                                                <span class="dim">
                                                    {format!("vouched by {}", friend.vouched_by.join(", "))}
                                                </span>
                                            </div>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .into_any()
                            }}
                            // My grouping: the keys clustered as this person.
                            <div class="dim">
                                {format!("{} device key(s) for this person", keys.len())}
                            </div>
                            {keys
                                .into_iter()
                                .map(|key| {
                                    let short = key.chars().take(16).collect::<String>();
                                    view! {
                                        <div class="dim" id="record-text">
                                            {format!("{short}…")}
                                        </div>
                                    }
                                })
                                .collect::<Vec<_>>()}
                            // Their relays (R5): what a message to them
                            // would use right now — provenance named, the
                            // manual override as the escape hatch.
                            <div class="dim">"their relays — where messages to them wait"</div>
                            <div class="row">
                                <span class="dim">{person.relay_source.clone()}</span>
                            </div>
                            {person
                                .relays
                                .clone()
                                .into_iter()
                                .map(|relay| {
                                    view! {
                                        <div class="dim" id="record-text">{relay.spec}</div>
                                        {relay
                                            .owed
                                            .map(|line| view! { <div class="dim">{line}</div> })}
                                    }
                                })
                                .collect::<Vec<_>>()}
                            <textarea
                                rows="2"
                                placeholder="override: paste relay spec(s) or a ZINK-RELAY code"
                                prop:value=move || override_input.get()
                                on:input=move |ev| override_input.set(event_target_value(&ev))
                            />
                            <button
                                class="secondary"
                                on:click=move |_| {
                                    let specs: Vec<String> = override_input
                                        .get_untracked()
                                        .split_whitespace()
                                        .map(str::to_string)
                                        .collect();
                                    if !specs.is_empty() {
                                        set_override(specs);
                                    }
                                }
                            >
                                "override their relays"
                            </button>
                            {person
                                .relay_override
                                .then(|| {
                                    view! {
                                        <button
                                            class="secondary"
                                            on:click=move |_| set_override(Vec::new())
                                        >
                                            "clear override — use their record"
                                        </button>
                                    }
                                })}
                            // Actions, in context. Vouching *is* sharing your
                            // name for them — say so plainly (the friends'
                            // lens above is the other side of this act).
                            <button on:click=move |_| toggle_vouch()>
                                {if person.vouched {
                                    "stop sharing your name for them"
                                } else {
                                    "share the name you call them"
                                }}
                            </button>
                            <div class="dim">
                                "lets friends who ask you about them see this name"
                            </div>
                            <button class="secondary" on:click=move |_| refresh()>
                                "refresh — who is this?"
                            </button>
                            {has_key
                                .then(|| {
                                    view! {
                                        <button
                                            class="danger"
                                            on:click=move |_| {
                                                if armed.get_untracked() {
                                                    repudiate();
                                                } else {
                                                    armed.set(true);
                                                }
                                            }
                                        >
                                            {move || {
                                                if armed.get() {
                                                    "⚠ confirm — this key isn't them"
                                                } else {
                                                    "this key isn't them anymore"
                                                }
                                            }}
                                        </button>
                                    }
                                })}
                        }
                            .into_any()
                    })
            }}
        </main>
    }
}
