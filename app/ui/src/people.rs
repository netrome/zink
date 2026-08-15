use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{AddPreview, AppState};

use crate::{NoArgs, avatar_data_url, invoke};

/// The R1 detour: a scanned/pasted record that belongs to an existing
/// contact, held for the user's explicit confirm before it replaces that
/// entry — the confirm card is the explicit act the overlap guard demands.
#[derive(Clone)]
struct PendingUpdate {
    payload: String,
    petname: String,
    changes: Vec<String>,
}

/// "People" — your contacts. The list is a plain, tappable list (a row opens
/// the person-detail lens, U4); adding a contact (scan / paste) hides behind a
/// "+" so the list stays clean, and the per-contact trust actions moved onto
/// the detail screen. Its scan always adds a contact; the device-pairing scan
/// lives in `MeView`.
#[component]
pub(crate) fn PeopleView(
    state: RwSignal<Option<AppState>>,
    reload: impl Fn() + Copy + Send + 'static,
    open_person: impl Fn(String) + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let paste = RwSignal::new(String::new());
    // Whether the add-contact form is open (vs the plain list).
    let adding = RwSignal::new(false);
    // Optional petname to set at add time (my lens); empty → their
    // self-claimed name. Applies to both the scan and paste paths.
    let new_name = RwSignal::new(String::new());

    // A scanned record matching an existing contact, awaiting the confirm
    // card (R1) — Some switches the composer to the card.
    let pending = RwSignal::new(None::<PendingUpdate>);

    let add = move |payload: String| {
        let petname = new_name.get_untracked();
        let petname = (!petname.trim().is_empty()).then_some(petname);
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
                petname: Option<&'a str>,
            }
            let args = Args {
                payload: &payload,
                petname: petname.as_deref(),
            };
            match invoke::invoke::<String>("add_contact", &args).await {
                Ok(petname) => {
                    paste.set(String::new());
                    new_name.set(String::new());
                    adding.set(false);
                    reload();
                    ok(&format!("added {petname}"));
                }
                Err(e) => err(e),
            }
        });
    };

    // Every scan/paste triages first (R1): a record overlapping a stored
    // contact detours to the confirm card instead of erroring on a petname
    // mismatch; a genuinely new one flows to the plain add.
    let submit = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
            }
            let args = Args { payload: &payload };
            match invoke::invoke::<AddPreview>("preview_contact", &args).await {
                Ok(preview) => match preview.updates {
                    Some(petname) => pending.set(Some(PendingUpdate {
                        payload,
                        petname,
                        changes: preview.changes,
                    })),
                    None => add(payload),
                },
                Err(e) => err(e),
            }
        });
    };

    let confirm_update = move |_| {
        let Some(update) = pending.get_untracked() else {
            return;
        };
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
            }
            let args = Args {
                payload: &update.payload,
            };
            match invoke::invoke::<String>("update_contact", &args).await {
                Ok(petname) => {
                    pending.set(None);
                    paste.set(String::new());
                    new_name.set(String::new());
                    adding.set(false);
                    reload();
                    ok(&format!("updated {petname}"));
                }
                Err(e) => err(e),
            }
        });
    };

    // Contact avatars, lazily fetched per row (same pattern as the chat).
    let contact_avatars = RwSignal::new(HashMap::<String, String>::new());
    Effect::new(move |_| {
        for contact in state.get().map(|state| state.contacts).unwrap_or_default() {
            let key = contact.key;
            if key.is_empty()
                || contact_avatars.with_untracked(|avatars| avatars.contains_key(&key))
            {
                continue;
            }
            contact_avatars.update(|avatars| {
                avatars.insert(key.clone(), String::new());
            });
            spawn_local(async move {
                if let Ok(Some(url)) = avatar_data_url(&key).await {
                    contact_avatars.update(|avatars| {
                        avatars.insert(key, url);
                    });
                }
            });
        }
    });

    // Scanning state drives the cancel overlay AND page transparency (see the
    // note in `MeView`). This scan always adds a contact.
    let scanning = RwSignal::new(false);
    Effect::new(move |_| {
        if let Some(root) = document().document_element() {
            root.set_class_name(if scanning.get() { "scanning" } else { "" });
        }
    });
    let scan = move |_| {
        scanning.set(true);
        spawn_local(async move {
            #[derive(Serialize)]
            struct ScanArgs {
                windowed: bool,
                formats: Vec<&'static str>,
            }
            #[derive(serde::Deserialize)]
            struct Scanned {
                content: String,
            }
            if let Err(e) = invoke::invoke::<serde::de::IgnoredAny>(
                "plugin:barcode-scanner|request_permissions",
                &NoArgs {},
            )
            .await
            {
                scanning.set(false);
                return err(e);
            }
            let args = ScanArgs {
                windowed: true,
                formats: vec!["QR_CODE"],
            };
            let result = invoke::invoke::<Scanned>("plugin:barcode-scanner|scan", &args).await;
            scanning.set(false);
            match result {
                Ok(scanned) => submit(scanned.content),
                // A cancelled scan also lands here — worth no red banner.
                Err(e) => err(e),
            }
        });
    };
    let cancel_scan = move |_| {
        spawn_local(async move {
            let _ = invoke::invoke::<serde::de::IgnoredAny>(
                "plugin:barcode-scanner|cancel",
                &NoArgs {},
            )
            .await;
        });
    };

    view! {
        <main>
            {move || {
                if let Some(update) = pending.get() {
                    // The update-confirm card (R1): the scanned code belongs
                    // to a stored contact — show what confirming changes.
                    view! {
                        <h3>"already a contact"</h3>
                        <div>"this code belongs to " <b>{update.petname.clone()}</b></div>
                        {if update.changes.is_empty() {
                            view! {
                                <div class="dim">"no name or relay changes"</div>
                            }
                                .into_any()
                        } else {
                            update
                                .changes
                                .iter()
                                .map(|change| {
                                    view! { <div class="dim">{change.clone()}</div> }
                                })
                                .collect::<Vec<_>>()
                                .into_any()
                        }}
                        <div class="dim">
                            {format!(
                                "your name for them stays “{}” — rename from their page",
                                update.petname,
                            )}
                        </div>
                        <button on:click=confirm_update>
                            {format!("update {}", update.petname)}
                        </button>
                        <button class="secondary" on:click=move |_| pending.set(None)>
                            "cancel"
                        </button>
                    }
                        .into_any()
                } else if adding.get() {
                    // Add-contact composer (scan / paste), off the "+".
                    view! {
                        <h3>"add contact"</h3>
                        <input
                            placeholder="your name for them (optional)"
                            prop:value=move || new_name.get()
                            on:input=move |ev| new_name.set(event_target_value(&ev))
                        />
                        <button on:click=scan>"scan QR"</button>
                        <textarea
                            rows="2"
                            placeholder="…or paste their code"
                            prop:value=move || paste.get()
                            on:input=move |ev| paste.set(event_target_value(&ev))
                        />
                        <button class="secondary" on:click=move |_| submit(paste.get_untracked())>
                            "add from pasted text"
                        </button>
                        <button
                            class="secondary"
                            on:click=move |_| {
                                adding.set(false);
                                paste.set(String::new());
                                new_name.set(String::new());
                            }
                        >
                            "cancel"
                        </button>
                    }
                        .into_any()
                } else {
                    // The plain list + the deliberate "+" to add a contact.
                    view! {
                        <button on:click=move |_| adding.set(true)>"+ add contact"</button>
                        {move || {
                            let contacts = state
                                .get()
                                .map(|state| state.contacts)
                                .unwrap_or_default();
                            if contacts.is_empty() {
                                view! {
                                    <div class="dim">
                                        "no contacts yet — tap + add contact to scan or paste a code"
                                    </div>
                                }
                                    .into_any()
                            } else {
                                contacts
                                    .into_iter()
                                    .map(|contact| {
                                        let petname = contact.petname.clone();
                                        let avatar_key = contact.key.clone();
                                        let has_warning = !contact.disavowals.is_empty();
                                        view! {
                                            <div
                                                class="row"
                                                on:click=move |_| open_person(petname.clone())
                                            >
                                                {move || {
                                                    contact_avatars
                                                        .with(|avatars| {
                                                            avatars
                                                                .get(&avatar_key)
                                                                .filter(|url| !url.is_empty())
                                                                .cloned()
                                                        })
                                                        .map(|url| view! { <img class="avatar" src=url /> })
                                                }}
                                                <b>{contact.petname}</b>
                                                {has_warning
                                                    .then(|| view! { <span class="dim">"⚠"</span> })}
                                            </div>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .into_any()
                            }
                        }}
                    }
                        .into_any()
                }
            }}
            {move || {
                scanning
                    .get()
                    .then(|| {
                        view! {
                            <div class="scan-overlay">
                                <span>"point the camera at a zink QR"</span>
                                <button class="secondary" on:click=cancel_scan>
                                    "cancel"
                                </button>
                            </div>
                        }
                    })
            }}
        </main>
    }
}
