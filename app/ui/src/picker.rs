use std::collections::{BTreeSet, HashMap};

use leptos::prelude::*;
use leptos::task::spawn_local;
use zink_app_dto::ContactRow;

use crate::avatar_data_url;

/// How many contacts before the picker grows a filter box.
const FILTER_AT: usize = 8;

/// The shared people-picker (project 6 S1): the selected set as removable
/// chips, then one full-width tappable row per contact — avatar + petname,
/// toggled by tapping anywhere on the row. Selection is by petname (the app
/// boundary's handle); what the picks *mean* is the caller's policy.
#[component]
pub(crate) fn PeoplePicker(
    contacts: Signal<Vec<ContactRow>>,
    selected: RwSignal<BTreeSet<String>>,
    /// Petnames to hide — e.g. people already in the conversation.
    #[prop(optional, into)]
    exclude: Option<Signal<Vec<String>>>,
) -> impl IntoView {
    let filter = RwSignal::new(String::new());

    // Contact avatars, lazily fetched per row (same pattern as PeopleView).
    let avatars = RwSignal::new(HashMap::<String, String>::new());
    Effect::new(move |_| {
        for contact in contacts.get() {
            let key = contact.key;
            if key.is_empty() || avatars.with_untracked(|avatars| avatars.contains_key(&key)) {
                continue;
            }
            avatars.update(|avatars| {
                avatars.insert(key.clone(), String::new());
            });
            spawn_local(async move {
                if let Ok(Some(url)) = avatar_data_url(&key).await {
                    avatars.update(|avatars| {
                        avatars.insert(key, url);
                    });
                }
            });
        }
    });

    view! {
        {move || {
            let chips = selected.get();
            (!chips.is_empty())
                .then(|| {
                    view! {
                        <div class="chips">
                            {chips
                                .into_iter()
                                .map(|name| {
                                    let removed = name.clone();
                                    view! {
                                        <span
                                            class="chip"
                                            on:click=move |_| {
                                                selected
                                                    .update(|selected| {
                                                        selected.remove(&removed);
                                                    })
                                            }
                                        >
                                            {name}
                                            " ×"
                                        </span>
                                    }
                                })
                                .collect::<Vec<_>>()}
                        </div>
                    }
                })
        }}
        {move || {
            (contacts.get().len() > FILTER_AT)
                .then(|| {
                    view! {
                        <input
                            placeholder="filter people"
                            prop:value=move || filter.get()
                            on:input=move |ev| filter.set(event_target_value(&ev))
                        />
                    }
                })
        }}
        {move || {
            let mut list = contacts.get();
            if list.is_empty() {
                return view! {
                    <div class="dim">
                        "no contacts yet — add people in the People tab first"
                    </div>
                }
                    .into_any();
            }
            let excluded = exclude.map(|exclude| exclude.get()).unwrap_or_default();
            list.retain(|contact| !excluded.contains(&contact.petname));
            if list.is_empty() {
                return view! {
                    <div class="dim">"everyone you know is already here"</div>
                }
                    .into_any();
            }
            list.sort_by(|a, b| a.petname.to_lowercase().cmp(&b.petname.to_lowercase()));
            let query = filter.get().trim().to_lowercase();
            list.retain(|contact| {
                query.is_empty() || contact.petname.to_lowercase().contains(&query)
            });
            list.into_iter()
                .map(|contact| {
                    let toggled = contact.petname.clone();
                    let checked = contact.petname.clone();
                    let picked = contact.petname.clone();
                    let avatar_key = contact.key.clone();
                    view! {
                        <div
                            class="row picker-row"
                            class:selected=move || {
                                selected.with(|selected| selected.contains(&checked))
                            }
                            on:click=move |_| {
                                selected
                                    .update(|selected| {
                                        if !selected.remove(&toggled) {
                                            selected.insert(toggled.clone());
                                        }
                                    })
                            }
                        >
                            {move || {
                                avatars
                                    .with(|avatars| {
                                        avatars
                                            .get(&avatar_key)
                                            .filter(|url| !url.is_empty())
                                            .cloned()
                                    })
                                    .map(|url| view! { <img class="avatar" src=url /> })
                            }}
                            <b>{contact.petname}</b>
                            {move || {
                                selected
                                    .with(|selected| selected.contains(&picked))
                                    .then(|| view! { <span class="check">"✓"</span> })
                            }}
                        </div>
                    }
                })
                .collect::<Vec<_>>()
                .into_any()
        }}
    }
}
