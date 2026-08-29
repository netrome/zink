use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{
    Conversation, DeviceCard, Key, PersonPage, PersonRef, RecordPreview, SubjectAsk, WhoIsReport,
};

use crate::chats::ConversationRow;
use crate::{avatar_data_url, image, invoke};

/// What the router hands the page (project 7 S3): a contact person by
/// opaque id, or a bare key — one page, two variants. Ids identify;
/// labels only display (the header renders from the fetched page, and a
/// rename never invalidates the target).
#[derive(Clone, PartialEq)]
pub(crate) enum PageTarget {
    Person(String),
    Key(Key),
}

/// The lens switcher (S3): my names · what they claim · through friends.
#[derive(Clone, Copy, PartialEq)]
enum Lens {
    Mine,
    Theirs,
    Friends,
}

async fn fetch_page(target: &PageTarget) -> Result<PersonPage, String> {
    match target {
        PageTarget::Person(id) => {
            #[derive(Serialize)]
            struct Args<'a> {
                id: &'a str,
            }
            invoke::invoke::<PersonPage>("person_page", &Args { id }).await
        }
        PageTarget::Key(key) => {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            invoke::invoke::<PersonPage>("key_page", &Args { key: key.as_str() }).await
        }
    }
}

/// The person page (S3, ui-design-system.md §1): one page for contacts and
/// strangers, keyed by the cluster lens. Header (avatar + label + acts),
/// the lens switcher, then one card per member device — labels, claims,
/// link evidence with direction, disavowal warnings, that device's relays,
/// and the fingerprint. Opening the page renders local stores only; its
/// one automatic network act is the rate-limited subject-refresh, which
/// asks nobody but the subject — a stranger's page fires nothing.
#[component]
pub(crate) fn PersonView(
    target: PageTarget,
    reload: impl Fn() + Copy + Send + 'static,
    back: impl Fn() + Copy + Send + 'static,
    open_chat: impl Fn(String, String) + Copy + Send + 'static,
    start_draft: impl Fn(Vec<String>) + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let target = StoredValue::new(target);
    let page = RwSignal::new(None::<PersonPage>);
    let avatar = RwSignal::new(None::<String>);
    let chats = RwSignal::new(Vec::<Conversation>::new());
    let lens = RwSignal::new(Lens::Mine);
    let rename_to = RwSignal::new(String::new());
    let merge_pick = RwSignal::new(String::new());
    let override_input = RwSignal::new(String::new());
    // Repudiation is armed-then-confirmed per key — it publishes.
    let armed = RwSignal::new(None::<String>);
    // The pair-back confirm (multi-device.md §3): decoded preview + the
    // payload it came from; nothing is signed until the explicit accept.
    let pair_preview = RwSignal::new(None::<(RecordPreview, String)>);

    // Conversations with this person — keyed by the page's person id, so
    // a key target that resolves to a person page lists them too.
    let load_chats = move |id: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                id: &'a str,
            }
            let args = Args { id: &id };
            if let Ok(list) =
                invoke::invoke::<Vec<Conversation>>("person_conversations", &args).await
            {
                chats.set(list);
            }
        });
    };

    let apply = move |loaded: PersonPage| {
        rename_to.set(loaded.label.clone());
        let avatar_key = loaded.avatar_key.clone();
        if let Some(info) = &loaded.person {
            load_chats(info.id.clone());
        }
        page.set(Some(loaded));
        spawn_local(async move {
            if !avatar_key.0.is_empty()
                && let Ok(url) = avatar_data_url(avatar_key.as_str()).await
            {
                avatar.set(url);
            }
        });
    };

    let load_page = move || {
        let current = target.get_value();
        spawn_local(async move {
            match fetch_page(&current).await {
                Ok(loaded) => {
                    // The page-open subject-refresh (contacts only,
                    // rate-limited): asks the subject about themself over
                    // an authenticated channel — no third party learns the
                    // page was opened. A heal re-renders with fresh data.
                    let refresh_keys = loaded.person.is_some().then(|| {
                        loaded
                            .devices
                            .iter()
                            .map(|card| card.key.clone())
                            .collect::<Vec<_>>()
                    });
                    apply(loaded);
                    if let Some(keys) = refresh_keys {
                        #[derive(Serialize)]
                        struct Args {
                            keys: Vec<Key>,
                        }
                        if matches!(
                            invoke::invoke::<bool>("page_refresh", &Args { keys }).await,
                            Ok(true)
                        ) && let Ok(fresh) = fetch_page(&current).await
                        {
                            apply(fresh);
                        }
                    }
                }
                Err(e) => err(e),
            }
        });
    };
    load_page();

    // Rename the person — the addressing label, my lens (S2). Acts key on
    // the page's person id; the id survives the label move, so the target
    // stays valid and the reload just re-renders.
    let do_rename = move || {
        let Some(current) = page.get_untracked() else {
            return;
        };
        let Some(info) = current.person else {
            return;
        };
        let new = rename_to.get_untracked();
        if new.trim().is_empty() || new == current.label {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                id: &'a str,
                new: &'a str,
            }
            let args = Args {
                id: &info.id,
                new: &new,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("rename_person", &args).await {
                Ok(_) => {
                    reload();
                    load_page();
                    ok(&format!("renamed to {new}"));
                }
                Err(e) => err(e),
            }
        });
    };

    // The explicit clustering act (S2): merge another person into this one.
    // Evidence only ever offers; this is the accept. The picker's value is
    // the person id; its label is only what the human saw.
    let do_merge = move || {
        let Some(current) = page.get_untracked() else {
            return;
        };
        let Some(info) = current.person else {
            return;
        };
        let from = merge_pick.get_untracked();
        if from.is_empty() {
            return;
        }
        let from_label = info
            .merge_candidates
            .iter()
            .find(|candidate| candidate.id == from)
            .map(|candidate| candidate.label.clone())
            .unwrap_or_default();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                into: &'a str,
                from: &'a str,
            }
            let args = Args {
                into: &info.id,
                from: &from,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("merge_persons", &args).await {
                Ok(_) => {
                    merge_pick.set(String::new());
                    reload();
                    load_page();
                    ok(&format!(
                        "{from_label} is now a device of {}",
                        current.label
                    ));
                }
                Err(e) => err(e),
            }
        });
    };

    // The undo of a merge: a member device becomes its own person again.
    let do_split = move |member: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                member: &'a str,
            }
            let args = Args { member: &member };
            match invoke::invoke::<String>("split_person", &args).await {
                Ok(label) => {
                    reload();
                    load_page();
                    ok(&format!("{label} is their own person again"));
                }
                Err(e) => err(e),
            }
        });
    };

    // Vouch per device entry (D4a) — what travels is the person label
    // (project 7 close-out), and the copy names it so nothing is shared
    // silently.
    let toggle_vouch = move |petname: String, vouched: bool| {
        // The published string: the page's person label, not the entry
        // petname the act is keyed by.
        let shared = page
            .get_untracked()
            .map(|current| current.label)
            .unwrap_or_else(|| petname.clone());
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
            }
            let args = Args { petname: &petname };
            let command = if vouched { "unvouch" } else { "vouch" };
            match invoke::invoke::<serde::de::IgnoredAny>(command, &args).await {
                Ok(_) => {
                    reload();
                    load_page();
                    ok(if vouched {
                        "no longer sharing a name for them".to_string()
                    } else {
                        format!(
                            "vouching — friends who ask you about them see \u{201c}{shared}\u{201d}"
                        )
                    }
                    .as_str());
                }
                Err(e) => err(e),
            }
        });
    };

    let repudiate = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("repudiate_key", &args).await {
                Ok(_) => {
                    armed.set(None);
                    reload();
                    load_page();
                    ok("marked compromised — published in your record; contacts \
                        learn it from their next pull");
                }
                Err(e) => err(e),
            }
        });
    };

    // Manual relay override per device entry (R5) — the escape hatch when
    // that device's record is stale and a rescan isn't at hand.
    let set_override = move |petname: String, relays: Vec<String>| {
        let cleared = relays.is_empty();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
                relays: &'a [String],
            }
            let args = Args {
                petname: &petname,
                relays: &relays,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("set_relay_override", &args).await {
                Ok(_) => {
                    override_input.set(String::new());
                    load_page();
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

    // A local photo (U6, my lens): only I see it; never published.
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
                    load_page();
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
                    avatar.set(avatar_data_url(&key).await.ok().flatten());
                    reload();
                    load_page();
                    ok("using their photo again");
                }
                Err(e) => err(e),
            }
        });
    };

    // Take a sibling's concurrent rename (S6, lens-sync.md §5): "use
    // theirs" is just a rename — my act, which supersedes and clears the
    // surfaced conflict.
    let use_theirs = move |theirs: String| {
        let Some(info) = page.get_untracked().and_then(|current| current.person) else {
            return;
        };
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                id: &'a str,
                new: &'a str,
            }
            let args = Args {
                id: &info.id,
                new: &theirs,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("rename_person", &args).await {
                Ok(_) => {
                    reload();
                    load_page();
                    ok(&format!("renamed to {theirs}"));
                }
                Err(e) => err(e),
            }
        });
    };

    // The photo share toggle (S5): publishes my photo of them as an
    // endorsement friends receive when they ask about this person —
    // explicit, like the vouch, with the copy owning what it shares.
    let toggle_share = move |key: String, sharing: bool| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            let command = if sharing {
                "unshare_avatar"
            } else {
                "share_avatar"
            };
            match invoke::invoke::<serde::de::IgnoredAny>(command, &args).await {
                Ok(_) => {
                    load_page();
                    ok(if sharing {
                        "no longer sharing your photo of them"
                    } else {
                        "sharing — friends who ask you about them see your photo"
                    });
                }
                Err(e) => err(e),
            }
        });
    };

    // Friends' shared photos (S5), lazily fetched per friend row — "as
    // they tell you", rendered only inside the through-friends lens.
    let friend_avatars = RwSignal::new(HashMap::<Key, String>::new());
    Effect::new(move |_| {
        let Some(current) = page.get() else {
            return;
        };
        for friend in current.friends {
            let Some(subject) = friend.avatar_of else {
                continue;
            };
            let key = friend.key.clone();
            if friend_avatars.with_untracked(|avatars| avatars.contains_key(&key)) {
                continue;
            }
            spawn_local(async move {
                #[derive(Serialize)]
                struct Args<'a> {
                    subject: &'a str,
                    friend: &'a str,
                }
                let args = Args {
                    subject: subject.as_str(),
                    friend: key.as_str(),
                };
                if let Ok(Some(b64)) =
                    invoke::invoke::<Option<String>>("friend_avatar", &args).await
                {
                    friend_avatars.update(|avatars| {
                        avatars.insert(key, image::data_url(&b64));
                    });
                }
            });
        }
    });

    // The per-friend scoped ask (S3): dials exactly this friend — they
    // learn you asked; nobody else does.
    let ask_friend = move |friend: String| {
        let Some(keys) = page.get_untracked().map(|page| {
            page.devices
                .iter()
                .map(|card| card.key.clone())
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                keys: Vec<Key>,
                friend: &'a str,
            }
            let args = Args {
                keys,
                friend: &friend,
            };
            match invoke::invoke::<WhoIsReport>("ask_friend", &args).await {
                Ok(report) if report.asked == 0 => {
                    err(format!("{friend} is not reachable right now"))
                }
                Ok(report) if report.answers == 0 => {
                    ok(&format!("asked {friend} — no record served"));
                    load_page();
                }
                Ok(_) => {
                    ok(&format!("asked {friend} — fresh answer applied"));
                    load_page();
                }
                Err(e) => err(e),
            }
        });
    };

    // The bootstrap's direct rung (S3b): ask the subject about themself —
    // the only query that works with no mutual contact (the onboarding
    // case). The dial is a liveness receipt, so the button copy owns it;
    // each outcome is worded distinctly, never collapsed to a shrug.
    let ask_them = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &key };
            match invoke::invoke::<SubjectAsk>("ask_subject", &args).await {
                Ok(SubjectAsk::Answered) => {
                    ok("they answered — review below, then add");
                    load_page();
                }
                Ok(SubjectAsk::Nothing) => {
                    ok("reached them — nothing served; they may not have added you")
                }
                Ok(SubjectAsk::Unreachable) => {
                    ok("couldn't reach them — try again when they're online")
                }
                Err(e) => err(e),
            }
        });
    };

    // The stranger bootstrap (D1b): ask every dialable contact — the one
    // deliberately broad query, and it stays a button.
    let who_is_everyone = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &key };
            match invoke::invoke::<WhoIsReport>("who_is", &args).await {
                Ok(report) => {
                    ok(&format!(
                        "{} answer(s) (asked {}, {} unreachable)",
                        report.answers, report.asked, report.unreachable
                    ));
                    load_page();
                }
                Err(e) => err(e),
            }
        });
    };

    let dismiss = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("dismiss", &args).await {
                Ok(_) => {
                    load_page();
                    ok("ignored — the key keeps rendering as itself");
                }
                Err(e) => err(e),
            }
        });
    };

    // Promote a candidate record to a contact (the explicit add). The
    // target stays the key — re-fetching lands on the person variant now.
    let add_candidate = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
                petname: Option<&'a str>,
            }
            let args = Args {
                payload: &payload,
                petname: None,
            };
            match invoke::invoke::<String>("add_contact", &args).await {
                Ok(petname) => {
                    reload();
                    ok(&format!("added {petname}"));
                    load_page();
                }
                Err(e) => err(e),
            }
        });
    };

    // Pair-back, step 1 (multi-device.md §3): decode and SHOW — name +
    // full-key fingerprint against the other device's me-view. Nothing is
    // signed until the explicit accept below.
    let preview_pair = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
            }
            let args = Args { payload: &payload };
            match invoke::invoke::<RecordPreview>("inspect_record", &args).await {
                Ok(preview) => pair_preview.set(Some((preview, payload))),
                Err(e) => err(e),
            }
        });
    };
    let confirm_pair = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
            }
            let args = Args { payload: &payload };
            match invoke::invoke::<String>("recognize_device", &args).await {
                Ok(name) => {
                    pair_preview.set(None);
                    reload();
                    ok(&format!(
                        "recognized {name} as your device — old messages re-wrap \
                         in the background"
                    ));
                    back();
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            <button class="secondary" on:click=move |_| back()>
                "‹ back"
            </button>
            {move || {
                page.get()
                    .map(|current| {
                        let is_person = current.person.is_some();
                        let label = current.label.clone();
                        let photo_key = current.avatar_key.0.clone();
                        let has_local = current.has_local_avatar;
                        let shares_my = current.shares_my_avatar;
                        let conflicts = current.conflicts.clone();
                        let devices = current.devices.clone();
                        let device_count = devices.len();
                        // The lens closure owns its own copy; the cards
                        // section below consumes `devices`.
                        let claim_devices = devices.clone();
                        let friends = current.friends.clone();
                        let merge_candidates = current
                            .person
                            .as_ref()
                            .map(|person| person.merge_candidates.clone())
                            .unwrap_or_default();
                        let stranger = current.stranger.clone();
                        view! {
                            // ── header: avatar + label + the person acts ──
                            <div class="pending">
                                {move || {
                                    avatar
                                        .get()
                                        .map(|url| view! { <img class="avatar avatar-lg" src=url /> })
                                }}
                                <h3>{label.clone()}</h3>
                                {stranger
                                    .is_some()
                                    .then(|| view! { <span class="dim">"— not a contact"</span> })}
                            </div>
                            {stranger
                                .as_ref()
                                .map(|info| {
                                    let key = info.key.0.clone();
                                    let candidates = info.candidates.clone();
                                    let dismissed = info.dismissed;
                                    let pair_back = info.pair_back.clone();
                                    let them_key = key.clone();
                                    let ask_key = key.clone();
                                    let dismiss_key = key.clone();
                                    view! {
                                        // ── the stranger acts: preview → decide ──
                                        {candidates
                                            .into_iter()
                                            .map(|candidate| {
                                                let (name, provenance, payload) = (
                                                    candidate.name,
                                                    candidate.provenance,
                                                    candidate.payload,
                                                );
                                                view! {
                                                    <div class="row">
                                                        <b>{name}</b>
                                                        <span class="dim">{provenance}</span>
                                                        {payload
                                                            .map(|payload| {
                                                                view! {
                                                                    <button on:click=move |_| add_candidate(
                                                                        payload.clone(),
                                                                    )>"add as contact"</button>
                                                                }
                                                            })}
                                                    </div>
                                                }
                                            })
                                            .collect::<Vec<_>>()}
                                        <button
                                            class="secondary"
                                            on:click=move |_| ask_them(them_key.clone())
                                        >
                                            "ask them who they are — they'll know you asked"
                                        </button>
                                        <button
                                            class="secondary"
                                            on:click=move |_| who_is_everyone(ask_key.clone())
                                        >
                                            "who is this? — asks all your contacts"
                                        </button>
                                        {(!dismissed)
                                            .then(|| {
                                                view! {
                                                    <button
                                                        class="secondary"
                                                        on:click=move |_| dismiss(dismiss_key.clone())
                                                    >
                                                        "ignore"
                                                    </button>
                                                }
                                            })}
                                        {pair_back
                                            .map(|payload| {
                                                view! {
                                                    <div class="row">
                                                        <span class="dim">
                                                            "this key says it's one of YOUR devices"
                                                        </span>
                                                    </div>
                                                    <button on:click=move |_| preview_pair(
                                                        payload.clone(),
                                                    )>"review & pair back"</button>
                                                }
                                            })}
                                        {move || {
                                            pair_preview
                                                .get()
                                                .map(|(preview, payload)| {
                                                    let shown = preview
                                                        .name
                                                        .unwrap_or_else(|| "(unnamed)".to_string());
                                                    view! {
                                                        // The one real risk (multi-device.md §3):
                                                        // confirm the fingerprint before signing.
                                                        <div class="row">
                                                            <b>{shown}</b>
                                                        </div>
                                                        <div class="dim" id="record-text">{preview.key.0.clone()}</div>
                                                        <div class="dim">
                                                            "compare this fingerprint with the key shown \
                                                             in the other device's Me view"
                                                        </div>
                                                        <button on:click=move |_| confirm_pair(
                                                            payload.clone(),
                                                        )>"this is my device — recognize it"</button>
                                                        <button
                                                            class="secondary"
                                                            on:click=move |_| pair_preview.set(None)
                                                        >
                                                            "cancel"
                                                        </button>
                                                    }
                                                })
                                        }}
                                    }
                                })}
                            // ── the lens switcher: three views of the same person ──
                            <div class="row">
                                <button
                                    class=move || {
                                        if lens.get() == Lens::Mine { "" } else { "secondary" }
                                    }
                                    on:click=move |_| lens.set(Lens::Mine)
                                >
                                    "my view"
                                </button>
                                <button
                                    class=move || {
                                        if lens.get() == Lens::Theirs { "" } else { "secondary" }
                                    }
                                    on:click=move |_| lens.set(Lens::Theirs)
                                >
                                    "what they claim"
                                </button>
                                <button
                                    class=move || {
                                        if lens.get() == Lens::Friends { "" } else { "secondary" }
                                    }
                                    on:click=move |_| lens.set(Lens::Friends)
                                >
                                    "through friends"
                                </button>
                            </div>
                            {move || {
                                match lens.get() {
                                    Lens::Mine => {
                                        let photo_key = photo_key.clone();
                                        let pick_key = photo_key.clone();
                                        let clear_key = photo_key.clone();
                                        let share_key = photo_key.clone();
                                        view! {
                                            {is_person
                                                .then(|| {
                                                    view! {
                                                        <div class="dim">"your name for them"</div>
                                                        <input
                                                            prop:value=move || rename_to.get()
                                                            on:input=move |ev| rename_to.set(event_target_value(&ev))
                                                        />
                                                        <button class="secondary" on:click=move |_| do_rename()>
                                                            "rename"
                                                        </button>
                                                    }
                                                })}
                                            // Sibling rename conflicts (S6):
                                            // provenance shown, nothing
                                            // arbitrates — taking theirs is
                                            // just a rename.
                                            {conflicts
                                                .iter()
                                                .map(|conflict| {
                                                    let theirs = conflict.theirs.clone();
                                                    let line = format!(
                                                        "your {} calls them \u{201c}{}\u{201d}",
                                                        conflict.from,
                                                        conflict.theirs,
                                                    );
                                                    view! {
                                                        <div class="row">
                                                            <span class="dim">{line}</span>
                                                            <button
                                                                class="secondary"
                                                                on:click=move |_| use_theirs(theirs.clone())
                                                            >
                                                                "use theirs"
                                                            </button>
                                                        </div>
                                                    }
                                                })
                                                .collect::<Vec<_>>()}
                                            {(!photo_key.is_empty())
                                                .then(|| {
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
                                                        // The photo share (S5), beside the
                                                        // vouch in spirit: explicit, and the
                                                        // copy owns what it publishes.
                                                        {(is_person && has_local)
                                                            .then(|| {
                                                                let share_key = share_key.clone();
                                                                view! {
                                                                    <button
                                                                        class="secondary"
                                                                        on:click=move |_| toggle_share(
                                                                            share_key.clone(),
                                                                            shares_my,
                                                                        )
                                                                    >
                                                                        {if shares_my {
                                                                            "stop sharing your photo of them"
                                                                        } else {
                                                                            "share your photo of them — friends who ask you see it"
                                                                        }}
                                                                    </button>
                                                                }
                                                            })}
                                                    }
                                                })}
                                        }
                                            .into_any()
                                    }
                                    Lens::Theirs => {
                                        let claims = claim_devices
                                            .iter()
                                            .map(|card| {
                                                let name = card
                                                    .self_name
                                                    .clone()
                                                    .unwrap_or_else(|| "—".to_string());
                                                let device = card
                                                    .device_label
                                                    .clone()
                                                    .map(|label| format!(" · {label}"))
                                                    .unwrap_or_default();
                                                view! {
                                                    <div class="row">
                                                        <b>{format!("{name}{device}")}</b>
                                                        <span class="dim">"their own claim, verified"</span>
                                                    </div>
                                                }
                                            })
                                            .collect::<Vec<_>>();
                                        view! { {claims} }.into_any()
                                    }
                                    Lens::Friends => {
                                        if friends.is_empty() {
                                            view! {
                                                <div class="row">
                                                    <span class="dim">
                                                        "no friend has told you anything about them yet"
                                                    </span>
                                                </div>
                                            }
                                                .into_any()
                                        } else {
                                            friends
                                                .clone()
                                                .into_iter()
                                                .map(|friend| {
                                                    let ask_name = friend.petname.clone();
                                                    let friend_key = friend.key.clone();
                                                    view! {
                                                        <div class="row">
                                                            // Their photo of this person (S5) —
                                                            // "as they tell you", never the
                                                            // page face.
                                                            {friend
                                                                .avatar_of
                                                                .is_some()
                                                                .then(|| {
                                                                    friend_avatars
                                                                        .with(|avatars| avatars.get(&friend_key).cloned())
                                                                        .map(|url| view! { <img class="avatar" src=url /> })
                                                                })}
                                                            <b>{friend.petname.clone()}</b>
                                                            <span class="dim">
                                                                {friend
                                                                    .vouched_name
                                                                    .map(|name| format!("calls them \u{201c}{name}\u{201d}"))
                                                                    .unwrap_or_else(|| "hasn't vouched a name".to_string())}
                                                            </span>
                                                            <button
                                                                class="secondary"
                                                                on:click=move |_| ask_friend(ask_name.clone())
                                                            >
                                                                "ask again"
                                                            </button>
                                                        </div>
                                                        {friend
                                                            .held
                                                            .into_iter()
                                                            .map(|line| view! { <div class="dim">{line}</div> })
                                                            .collect::<Vec<_>>()}
                                                    }
                                                })
                                                .collect::<Vec<_>>()
                                                .into_any()
                                        }
                                    }
                                }
                            }}
                            // Reactive on the lens: the disclosure must be
                            // readable BEFORE the first ask, not after.
                            {move || {
                                matches!(lens.get(), Lens::Friends)
                                    .then(|| {
                                        view! {
                                            <div class="dim">
                                                "asking a friend dials only them — they'll know you asked"
                                            </div>
                                        }
                                    })
                            }}
                            // ── conversations (contact case) ──
                            {is_person
                                .then(|| {
                                    let draft_label = label.clone();
                                    view! {
                                        <div class="dim">"conversations with them"</div>
                                        {move || {
                                            let list = chats.get();
                                            if list.is_empty() {
                                                view! {
                                                    <div class="row">
                                                        <span class="dim">"none yet"</span>
                                                    </div>
                                                }
                                                    .into_any()
                                            } else {
                                                list.into_iter()
                                                    .map(|conversation| {
                                                        view! {
                                                            <ConversationRow
                                                                conversation=conversation
                                                                open=open_chat
                                                            />
                                                        }
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .into_any()
                                            }
                                        }}
                                        <button on:click=move |_| {
                                            start_draft(vec![draft_label.clone()])
                                        }>"start a new conversation"</button>
                                    }
                                })}
                            // ── the devices: one card per member entry ──
                            <div class="dim">
                                {if device_count == 1 {
                                    "their device".to_string()
                                } else {
                                    format!("their devices ({device_count})")
                                }}
                            </div>
                            {devices
                                .into_iter()
                                .map(|card| {
                                    device_card_view(
                                        card,
                                        is_person,
                                        label.clone(),
                                        armed,
                                        override_input,
                                        toggle_vouch,
                                        do_split,
                                        set_override,
                                        repudiate,
                                    )
                                })
                                .collect::<Vec<_>>()}
                            // ── clustering: the explicit merge act (S2) ──
                            {(is_person && !merge_candidates.is_empty())
                                .then(|| {
                                    view! {
                                        <div class="dim">
                                            "same person as… (merges another entry's devices into this one)"
                                        </div>
                                        <div class="row">
                                            <select on:change=move |ev| {
                                                merge_pick.set(event_target_value(&ev))
                                            }>
                                                <option value="">"choose…"</option>
                                                {merge_candidates
                                                    .into_iter()
                                                    .map(|candidate| {
                                                        let PersonRef { id, label } = candidate;
                                                        view! { <option value=id>{label}</option> }
                                                    })
                                                    .collect::<Vec<_>>()}
                                            </select>
                                            <button class="secondary" on:click=move |_| do_merge()>
                                                "merge"
                                            </button>
                                        </div>
                                    }
                                })}
                        }
                            .into_any()
                    })
            }}
        </main>
    }
}

/// One member device card: my petname layer, their claims, the link
/// evidence with direction, warnings, that device's relays (+ the R5
/// override for contacts), and the trust acts in context.
#[allow(clippy::too_many_arguments)]
fn device_card_view(
    card: DeviceCard,
    is_person: bool,
    shared_label: String,
    armed: RwSignal<Option<String>>,
    override_input: RwSignal<String>,
    toggle_vouch: impl Fn(String, bool) + Copy + Send + 'static,
    do_split: impl Fn(String) + Copy + Send + 'static,
    set_override: impl Fn(String, Vec<String>) + Copy + Send + 'static,
    repudiate: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    // The card's acts and fingerprint work on the bare hex — the display
    // and invoke-arg form; the nominal `Key` stops at the dto boundary.
    let key = card.key.0.clone();
    let armed_key = key.clone();
    let repudiate_key = key.clone();
    let vouch_name = card.petname.clone();
    let split_name = card.petname.clone();
    let override_name = card.petname.clone();
    let clear_name = card.petname.clone();
    let title = match (&card.device_label, is_person) {
        (Some(label), _) => format!("{} · {label}", card.petname),
        (None, _) => card.petname.clone(),
    };
    view! {
        <div class="wild">
            <div class="row">
                <b>{title}</b>
            </div>
            {card
                .self_name
                .clone()
                .map(|name| {
                    view! {
                        <div class="dim">{format!("calls themself \u{201c}{name}\u{201d}")}</div>
                    }
                })}
            {card
                .link
                .clone()
                .into_iter()
                .map(|line| view! { <div class="dim">{line}</div> })
                .collect::<Vec<_>>()}
            {card
                .disavowals
                .clone()
                .into_iter()
                .map(|line| view! { <div class="dim">{line}</div> })
                .collect::<Vec<_>>()}
            // The fingerprint — legible mono, at the trust surface.
            <div class="dim" id="record-text">{key.clone()}</div>
            // That device's relays: they bind to the publishing device
            // (SPEC §3.6) — never a person-level list.
            <div class="dim">{card.relay_source.clone()}</div>
            {card
                .relays
                .clone()
                .into_iter()
                .map(|relay| {
                    view! {
                        <div class="dim" id="record-text">{relay.spec}</div>
                        {relay.owed.map(|line| view! { <div class="dim">{line}</div> })}
                    }
                })
                .collect::<Vec<_>>()}
            {is_person
                .then(|| {
                    view! {
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
                                    set_override(override_name.clone(), specs);
                                }
                            }
                        >
                            "override this device's relays"
                        </button>
                        {card
                            .relay_override
                            .then(|| {
                                let clear_name = clear_name.clone();
                                view! {
                                    <button
                                        class="secondary"
                                        on:click=move |_| set_override(clear_name.clone(), Vec::new())
                                    >
                                        "clear override — use their record"
                                    </button>
                                }
                            })}
                        <button on:click={
                            let vouched = card.vouched;
                            move |_| toggle_vouch(vouch_name.clone(), vouched)
                        }>
                            {if card.vouched {
                                "stop sharing your name for this device".to_string()
                            } else {
                                // What travels is the person label (the
                                // published claim), never the entry petname.
                                format!("share \u{201c}{shared_label}\u{201d} with friends who ask")
                            }}
                        </button>
                        {card
                            .can_split
                            .then(|| {
                                view! {
                                    <button
                                        class="secondary"
                                        on:click=move |_| do_split(split_name.clone())
                                    >
                                        "not the same person — split this device out"
                                    </button>
                                }
                            })}
                        <button
                            class="danger"
                            on:click=move |_| {
                                if armed.get_untracked().as_deref() == Some(&armed_key) {
                                    repudiate(repudiate_key.clone());
                                } else {
                                    armed.set(Some(armed_key.clone()));
                                    // Armed-forever is a footgun (S4): an
                                    // untouched confirm disarms itself.
                                    set_timeout(
                                        move || armed.set(None),
                                        std::time::Duration::from_secs(4),
                                    );
                                }
                            }
                        >
                            {
                                let key = key.clone();
                                move || {
                                    if armed.get().as_deref() == Some(&key) {
                                        "⚠ confirm — this key isn't them"
                                    } else {
                                        "this key isn't them anymore"
                                    }
                                }
                            }
                        </button>
                    }
                })}
        </div>
    }
}
