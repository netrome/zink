use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{AddPreview, AppState, Key, RELAY_QR_PREFIX, SiblingOffer};

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

/// The add-contact form's in-progress edits (S4, U8): App-lifetime signals,
/// so a tab bounce (which remounts the view) can't destroy typing.
#[derive(Clone, Copy)]
pub(crate) struct AddContactForm {
    pub adding: RwSignal<bool>,
    pub paste: RwSignal<String>,
    pub new_name: RwSignal<String>,
}

impl Default for AddContactForm {
    fn default() -> Self {
        Self {
            adding: RwSignal::new(false),
            paste: RwSignal::new(String::new()),
            new_name: RwSignal::new(String::new()),
        }
    }
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
    form: AddContactForm,
    open_person: impl Fn(String) + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    // `adding` is whether the form is open (vs the plain list); `new_name`
    // is the optional petname to set at add time (my lens; empty → their
    // self-claimed name), for both the scan and paste paths.
    let AddContactForm {
        adding,
        paste,
        new_name,
    } = form;

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
    // mismatch; a genuinely new one flows to the plain add. A relay code
    // (R4) is redirected — relays are yours, not a person.
    let submit = move |payload: String| {
        if payload.trim().starts_with(RELAY_QR_PREFIX) {
            return err("that's a relay code — add it from your page (me → your relays)".into());
        }
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
    let contact_avatars = RwSignal::new(HashMap::<Key, String>::new());
    Effect::new(move |_| {
        for contact in state.get().map(|state| state.contacts).unwrap_or_default() {
            let key = contact.key;
            if key.0.is_empty()
                || contact_avatars.with_untracked(|avatars| avatars.contains_key(&key))
            {
                continue;
            }
            contact_avatars.update(|avatars| {
                avatars.insert(key.clone(), String::new());
            });
            spawn_local(async move {
                if let Ok(Some(url)) = avatar_data_url(key.as_str()).await {
                    contact_avatars.update(|avatars| {
                        avatars.insert(key, url);
                    });
                }
            });
        }
    });

    // Offers from sibling devices (S6, lens-sync.md §6): rendered with
    // provenance; only the explicit accept below writes the contact store.
    let offers = RwSignal::new(Vec::<SiblingOffer>::new());
    let load_offers = move || {
        spawn_local(async move {
            if let Ok(list) = invoke::invoke::<Vec<SiblingOffer>>("lens_offers", &NoArgs {}).await {
                offers.set(list);
            }
        });
    };
    load_offers();
    let accept = move |subject: Key| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args {
                subject: subject.as_str(),
            };
            match invoke::invoke::<String>("accept_offer", &args).await {
                Ok(petname) => {
                    reload();
                    load_offers();
                    ok(&format!("added {petname}"));
                }
                Err(e) => err(e),
            }
        });
    };
    let decline = move |subject: Key| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args {
                subject: subject.as_str(),
            };
            match invoke::invoke::<serde::de::IgnoredAny>("decline_offer", &args).await {
                Ok(_) => {
                    load_offers();
                    ok("dismissed — your other device keeps its own copy");
                }
                Err(e) => err(e),
            }
        });
    };

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
                Err(e) => crate::scan_failed(err, e),
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
                        // Sibling offers (S6): "your phone added X — add
                        // them here too?" Accept is the only write.
                        {move || {
                            let pending = offers.get();
                            (!pending.is_empty())
                                .then(|| {
                                    view! {
                                        <div class="panel">
                                            {pending
                                                .into_iter()
                                                .map(|offer| {
                                                    let accept_key = offer.subject.clone();
                                                    let decline_key = offer.subject.clone();
                                                    view! {
                                                        <div class="row">
                                                            <span class="dim">
                                                                {format!(
                                                                    "your {} added {} — add them here too?",
                                                                    offer.from,
                                                                    offer.petname,
                                                                )}
                                                            </span>
                                                            <button on:click=move |_| accept(
                                                                accept_key.clone(),
                                                            )>"add"</button>
                                                            <button
                                                                class="secondary"
                                                                on:click=move |_| decline(decline_key.clone())
                                                            >
                                                                "dismiss"
                                                            </button>
                                                        </div>
                                                    }
                                                })
                                                .collect::<Vec<_>>()}
                                        </div>
                                    }
                                })
                        }}
                        {move || {
                            let mut contacts = state
                                .get()
                                .map(|state| state.contacts)
                                .unwrap_or_default();
                            // Alphabetical, guaranteed (S5) — same order as
                            // the picker.
                            contacts.sort_by(|a, b| {
                                a.petname.to_lowercase().cmp(&b.petname.to_lowercase())
                            });
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
                                        // The row opens by person id — the
                                        // stable handle; the label is display.
                                        let id = contact.id.clone();
                                        let avatar_key = contact.key.clone();
                                        let has_warning = !contact.disavowals.is_empty();
                                        // Their self-claim, dim, only when
                                        // it adds anything (S5).
                                        let self_name = contact
                                            .self_name
                                            .clone()
                                            .filter(|name| *name != contact.petname);
                                        view! {
                                            <div
                                                class="row"
                                                on:click=move |_| open_person(id.clone())
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
                                                {(contact.members > 1)
                                                    .then(|| {
                                                        view! {
                                                            <span class="dim">
                                                                {format!("{} devices", contact.members)}
                                                            </span>
                                                        }
                                                    })}
                                                {has_warning
                                                    .then(|| view! { <span class="dim">"⚠"</span> })}
                                                {self_name
                                                    .map(|name| {
                                                        view! {
                                                            <div class="dim">
                                                                {format!("calls themselves {name}")}
                                                            </div>
                                                        }
                                                    })}
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
