use nanocode::{HarnessFamily, ModelSelection, ModelTransport, ServiceTier, Thinking};
use nanocode_tui::{
    effort::{EffortDial, Level},
    picker::{ItemKind, Picker, PickerItem},
    theme::Theme,
};

pub fn catalog() -> Vec<(HarnessFamily, Vec<ModelSelection>)> {
    HarnessFamily::builtin()
        .into_iter()
        .map(|family| {
            let models = family.models().collect();
            (family, models)
        })
        .collect()
}

fn harness_title(family: &HarnessFamily) -> String {
    if *family == HarnessFamily::Claude {
        "Claude".into()
    } else {
        "Codex".into()
    }
}

pub fn features(model: &ModelSelection) -> String {
    let capabilities = model.capabilities(ModelTransport::Native);
    let mut features = Vec::new();
    for tier in capabilities
        .service_tiers()
        .filter(|tier| *tier != ServiceTier::Standard)
    {
        features.push(tier.as_str());
    }
    features.join(" · ")
}

pub fn model_picker(current: &ModelSelection, gateway: Option<&crate::host::Gateway>) -> Picker {
    let mut items = Vec::new();
    let mut sections = catalog()
        .into_iter()
        .map(|(family, models)| (harness_title(&family), models))
        .collect::<Vec<_>>();
    if let Some(gateway) = gateway.filter(|gateway| !gateway.models().is_empty()) {
        sections.insert(0, (gateway.name().to_owned(), gateway.models()));
    }
    for (title, models) in sections {
        if models
            .iter()
            .all(|model| gateway.is_some_and(|gateway| !gateway.serves(model)))
        {
            continue;
        }
        items.push(PickerItem::header(title));
        for model in models {
            if gateway.is_some_and(|gateway| !gateway.serves(&model)) {
                continue;
            }
            let thinking = crate::host::thinking_levels(&model)
                .into_iter()
                .map(Thinking::as_str)
                .collect::<Vec<_>>()
                .join("/");
            let mut item = PickerItem::choice(model.to_string())
                .detail(format!(
                    "{} · thinking {thinking}",
                    crate::host::display_name(&model)
                ))
                .current(model == *current);
            let features = features(&model);
            if !features.is_empty() {
                item = item.badge(features);
            }
            items.push(item);
        }
    }
    Picker::new("Model", items)
        .filterable()
        .footer("type to filter · enter select · esc cancel")
}

pub fn model_at(picker: &Picker, index: usize) -> Option<ModelSelection> {
    let item = picker.items().get(index)?;
    (item.kind == ItemKind::Choice).then(|| crate::host::parse_model(&item.label).ok())?
}

pub fn thinking_picker(model: &ModelSelection, current: Thinking) -> Picker {
    let default = crate::host::default_thinking(model);
    let items = crate::host::thinking_levels(model)
        .into_iter()
        .map(|thinking| {
            let mut item = PickerItem::choice(thinking.as_str()).current(thinking == current);
            if thinking == default {
                item = item.detail("default");
            }
            item
        })
        .collect();
    Picker::new(format!("Thinking · {model}"), items).footer("enter select · esc cancel")
}

pub fn thinking_at(model: &ModelSelection, picker: &Picker, index: usize) -> Option<Thinking> {
    let label = &picker.items().get(index)?.label;
    crate::host::thinking_levels(model)
        .into_iter()
        .find(|thinking| thinking.as_str() == label)
}

pub fn tier_options(model: &ModelSelection) -> Vec<ServiceTier> {
    model
        .capabilities(ModelTransport::Native)
        .service_tiers()
        .collect()
}

pub fn tier_picker(model: &ModelSelection, tiers: &[ServiceTier], current: ServiceTier) -> Picker {
    let items = tiers
        .iter()
        .map(|tier| {
            PickerItem::choice(tier.as_str())
                .detail(match tier {
                    ServiceTier::Standard => "default speed",
                    ServiceTier::Fast | ServiceTier::Priority => "priority processing",
                    ServiceTier::Ultrafast => "lowest latency",
                })
                .current(*tier == current)
        })
        .collect();
    Picker::new(format!("Speed · {model}"), items).footer("enter select · esc cancel")
}

pub const fn thinking_color(
    thinking: Thinking,
    theme: &Theme,
) -> nanocode_tui::ratatui::style::Color {
    match thinking {
        Thinking::None => theme.muted(),
        Thinking::Low => theme.thinking_low(),
        Thinking::Medium => theme.thinking_medium(),
        Thinking::High => theme.thinking_high(),
        Thinking::Xhigh => theme.thinking_xhigh(),
        Thinking::Max => theme.thinking_max(),
    }
}

pub fn thinking_dial(
    model: &ModelSelection,
    current: Thinking,
    theme: &Theme,
) -> Option<(EffortDial, Vec<Thinking>)> {
    let levels = crate::host::thinking_levels(model);
    let selected = levels
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    let dial = EffortDial::new(
        format!("Thinking · {}", crate::host::display_name(model)),
        levels
            .iter()
            .map(|level| Level {
                label: level.as_str().to_owned(),
                color: thinking_color(*level, theme),
            })
            .collect(),
        selected,
    )?;
    Some((dial, levels))
}
