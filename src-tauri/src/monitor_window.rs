//! Native Slint receiver: GPU vector UI, no WebView/browser or Windows controls.
use crate::native_monitor::{View, Workers};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver as EventReceiver, TryRecvError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use sysx_ui::layout::{fit_preview_image, fov_diameter, map_detection_box, map_detection_point};

slint::slint! {
    import { ScrollView } from "std-widgets.slint";
    export struct MouseKey { label: string, known: bool, down: bool }
    export struct ConfigOption { label: string, value: string }
    export struct ConfigField { key: string, label: string, value: string, kind: string, choices: [ConfigOption] }
    component ConfigAction inherits FocusScope {
        in property <string> caption;
        in property <bool> active;
        in property <bool> primary;
        in property <bool> danger;
        in property <length> control-height: 34px;
        callback activated();
        height: root.control-height;
        key-pressed(event) => {
            if !event.repeat && (event.text == Key.Return || event.text == Key.Space) {
                root.activated();
                accept
            } else {
                reject
            }
        }
        Rectangle {
            width: parent.width; height: parent.height;
            border-radius: 5px; border-width: root.has-focus ? 2px : 1px;
            border-color: root.has-focus ? #e8b858 : (root.danger ? #ef6c6c : (root.active || root.primary ? #43cfa0 : #344454));
            background: root.primary ? #43cfa0 : (root.danger ? #ef6c6c12 : (root.active ? #43cfa01a : #0f151c));
            Text {
                text: root.caption;
                color: root.primary ? #08120f : (root.danger ? #ef6c6c : #ebf1f5);
                font-family: "Segoe UI";
                font-size: root.primary ? 13px : 12px;
                font-weight: root.primary ? 700 : 400;
                horizontal-alignment: root.primary ? center : left;
                vertical-alignment: center;
                x: root.primary ? 0px : 9px;
                width: parent.width - (root.primary ? 0px : 18px);
            }
            TouchArea {
                clicked => { root.focus(); root.activated(); }
            }
        }
    }
    export component Receiver inherits Window {
        title: "SysX";
        icon: @image-url("icons/icon.png");
        preferred-width: 550px; preferred-height: 620px;
        min-width: 360px; min-height: 420px;
        background: #090c10;
        in property <string> body-font: "Segoe UI";
        in property <string> mono-font: "Consolas";
        default-font-family: root.body-font;
        default-font-size: 13px;
        in property <string> profile: "Default";
        in property <string> device: "No device";
        in property <color> device-ink: #9aa9b4;
        in property <string> status: "Starting receiver";
        in property <color> status-ink: #e8b858;
        in property <string> detail;
        in property <string> resolution: "—";
        in property <string> fps: "—";
        in property <string> latency: "—";
        in property <color> fps-ink: #ebf1f5;
        in property <color> latency-ink: #ebf1f5;
        in property <bool> high-latency;
        in property <image> preview;
        in property <bool> has-frame;
        in property <bool> pixel-perfect;
        in property <float> image-x;
        in property <float> image-y;
        in property <float> image-width;
        in property <float> image-height;
        in property <float> fov-x;
        in property <float> fov-y;
        in property <float> fov-size;
        in property <color> fov-ink: #43cfa0;
        in property <image> overlay;
        in property <[MouseKey]> mouse-keys;
        in property <string> detection-summary;
        in property <bool> has-target;
        in property <image> target-layer;
        in property <float> target-x;
        in property <float> target-y;
        in property <float> target-w;
        in property <float> target-h;
        in property <color> target-ink: #e3bf4d;
        out property <float> feed-size: feed.width / 1px;
        callback settings-requested();
        callback viewport-changed();
        changed feed-size => { root.viewport-changed(); }
        Rectangle {
            x: 0px; y: 0px; width: parent.width; height: 58px; background: #090c10;
            Rectangle {
                x: 12px; y: 9px; width: parent.width - 24px; height: 40px;
                border-radius: 8px; border-width: 1px; border-color: #2d3946; background: #0f151c;
            }
            Image { x: 22px; y: 17px; width: 24px; height: 24px; source: @image-url("../public/receiver-mark.svg"); }
            HorizontalLayout {
                x: 58px; width: parent.width - self.x - 22px;
                y: 15px; height: 28px; spacing: 8px;
                Rectangle {
                    horizontal-stretch: 1; border-radius: 5px; border-width: 1px; border-color: #263340; background: #121a23;
                    HorizontalLayout { padding-left: 10px; padding-right: 10px; spacing: 7px;
                        if root.width > 460px: Text { text: "ACTIVE PROFILE"; color: #9aa9b4; font-size: 10px; vertical-alignment: center; }
                        Text { text: root.profile; color: #ebf1f5; font-size: 12px; font-weight: 600; overflow: elide; vertical-alignment: center; }
                    }
                }
                Rectangle { width: root.width > 430px ? 122px : 92px; border-radius: 5px; border-width: 1px; border-color: root.device-ink.with-alpha(0.42); background: root.device-ink.with-alpha(0.10);
                    HorizontalLayout { padding-left: 9px; padding-right: 8px; spacing: 7px;
                        Rectangle { width: 7px; height: 7px; y: 10px; border-radius: 4px; background: root.device-ink; }
                        Text { text: root.device; color: #ebf1f5; font-size: 12px; vertical-alignment: center; overflow: elide; }
                    }
                }
                ConfigAction {
                    width: 70px; control-height: 28px; caption: "Settings";
                    activated => { root.settings-requested(); }
                }
            }
        }
        property <length> pad: root.width <= 520px ? 12px : 20px;
        feed := Rectangle {
            width: min(640px, root.width - 2 * root.pad, root.height - 58px - 2 * root.pad - 40px);
            height: self.width; x: (root.width - self.width) / 2;
            y: 58px + (root.height - 58px - self.height - 40px) / 2;
            border-radius: 8px; border-width: 1px; border-color: #2d3946; background: #05080b; clip: true;
            if root.has-frame: Image {
                x: root.image-x * 1px; y: root.image-y * 1px;
                width: root.image-width * 1px; height: root.image-height * 1px;
                source: root.preview; image-fit: fill;
                image-rendering: root.pixel-perfect ? pixelated : smooth;
            }
            if root.has-frame: Rectangle {
                x: root.fov-x * 1px; y: root.fov-y * 1px; width: root.fov-size * 1px; height: self.width;
                border-radius: self.width / 2; border-width: 1px; border-color: root.fov-ink.with-alpha(0.8); background: transparent;
            }
            if root.has-frame: Image { x: 0px; y: 0px; width: parent.width; height: parent.height; source: root.overlay; image-fit: fill; }
            if root.has-frame && root.has-target: Image {
                x: root.target-x * 1px; y: root.target-y * 1px; width: root.target-w * 1px; height: root.target-h * 1px;
                source: root.target-layer; image-fit: fill;
            }
            if root.has-frame && root.detection-summary != "": Rectangle {
                x: 10px; y: parent.height - 39px; width: parent.width - 20px; height: 29px;
                background: #06090cdb; border-width: 1px; border-color: root.target-ink;
                Text { text: root.detection-summary; color: #ebf1f5; font-family: root.mono-font; font-size: 12px; horizontal-alignment: center; vertical-alignment: center; overflow: elide; }
            }
            if !root.has-frame: VerticalLayout {
                alignment: center; padding: 24px; spacing: 10px;
                Text { text: "NO FRAME"; color: #9aa9b4; font-family: root.mono-font; font-size: 11px; horizontal-alignment: center; }
                HorizontalLayout { alignment: center; spacing: 8px;
                    Rectangle { width: 7px; height: 7px; border-radius: 4px; background: root.status-ink; y: 8px; }
                    Text { text: root.status; color: #ebf1f5; font-size: 16px; font-weight: 600; }
                }
                if root.detail != "": Text { text: root.detail; color: #9aa9b4; font-size: 12px; horizontal-alignment: center; wrap: word-wrap; }
            }
            Rectangle { x: 10px; y: 10px; height: 29px; width: min(parent.width - 20px, root.high-latency ? 332px : 290px); border-radius: 5px; background: #06090cdb;
                HorizontalLayout { padding-left: 10px; padding-right: 10px; spacing: 14px;
                    HorizontalLayout { spacing: 4px;
                        Text { text: "Res:"; color: #9aa9b4; font-size: 11px; vertical-alignment: center; }
                        Text { text: root.resolution; color: #ebf1f5; font-family: root.mono-font; font-size: 12px; vertical-alignment: center; }
                    }
                    HorizontalLayout { spacing: 4px;
                        Text { text: root.fps; color: root.fps-ink; font-family: root.mono-font; font-size: 12px; vertical-alignment: center; }
                        Text { text: "FPS"; color: #9aa9b4; font-size: 11px; vertical-alignment: center; }
                    }
                    HorizontalLayout { spacing: 4px;
                        Text { text: root.latency; color: root.latency-ink; font-family: root.mono-font; font-size: 12px; vertical-alignment: center; }
                        Text { text: "ms"; color: #9aa9b4; font-size: 11px; vertical-alignment: center; }
                        if root.high-latency: Text { text: "(HIGH)"; color: #ef6c6c; font-family: root.mono-font; font-size: 10px; vertical-alignment: center; }
                    }
                }
            }
        }
        HorizontalLayout {
            x: feed.x; y: feed.y + feed.height + 10px; width: feed.width; height: 30px; spacing: 4px;
            for key in root.mouse-keys: Rectangle {
                horizontal-stretch: 1; background: key.down ? #43cfa029 : #121a23; border-width: 1px; border-color: #2d3946; border-radius: 5px;
                Text { text: key.label; color: key.known ? #ebf1f5 : #53616c; font-size: 12px; horizontal-alignment: center; vertical-alignment: center; }
                if key.down: Rectangle { x: 5px; y: parent.height - 2px; height: 2px; width: parent.width - 10px; background: #43cfa0; }
            }
        }
    }

    export component SettingsWindow inherits Window {
        title: "SysX Settings";
        preferred-width: 760px; preferred-height: 720px;
        min-width: 620px; min-height: 560px;
        background: #090c10;
        in property <string> body-font: "Segoe UI";
        in property <string> mono-font: "Consolas";
        in property <[ConfigField]> fields;
        in property <string> message;
        callback field-edited(string, string);
        callback field-activated(string);
        callback field-selected(string, string);
        callback apply-requested();
        callback close-requested();
        callback reset-scroll();
        reset-scroll => { settings-list.viewport-y = 0px; }
            VerticalLayout {
                padding: 18px; spacing: 10px;
            HorizontalLayout {
                height: 48px; spacing: 10px;
                VerticalLayout { spacing: 2px;
                    Text { text: "SYSX  /  SETTINGS"; color: #43cfa0; font-size: 10px; font-weight: 700; }
                    Text { text: "Configuration"; color: #ebf1f5; font-size: 20px; font-weight: 600; }
                }
                Rectangle { horizontal-stretch: 1; }
                if root.fields.length > 12: Text { text: "SCROLL FOR MORE"; color: #9aa9b4; font-family: root.mono-font; font-size: 10px; vertical-alignment: center; }
                ConfigAction {
                    width: 70px; control-height: 32px; caption: "Close";
                    activated => { root.close-requested(); }
                }
            }
            Rectangle { height: 1px; background: #263340; }
            settings-list := ScrollView {
                horizontal-stretch: 1; vertical-stretch: 1;
                vertical-scrollbar-policy: ScrollBarPolicy.always-on;
                VerticalLayout {
                        width: parent.width; height: root.fields.length * 50px; spacing: 4px;
                        for field in root.fields: Rectangle {
                            width: parent.width; height: 46px;
                            background: field.kind == "section" ? #0e151c : #090c10;
                            border-radius: field.kind == "section" ? 4px : 0px;
                            border-width: field.kind == "section" ? 1px : 0px;
                            border-color: #263340;
                            if field.kind == "section": HorizontalLayout {
                                x: 10px; y: 4px; width: parent.width - 20px; height: 30px; spacing: 10px;
                                Text { text: field.label; color: #43cfa0; font-size: 11px; font-weight: 700; vertical-alignment: center; }
                                Rectangle { horizontal-stretch: 1; height: 1px; background: #263340; }
                            }
                            if field.kind == "engine": HorizontalLayout {
                                spacing: 8px;
                                Rectangle { horizontal-stretch: 1; height: 34px; border-radius: 5px; border-width: 1px; border-color: #43cfa0; background: #43cfa01a;
                                    HorizontalLayout { padding-left: 10px; padding-right: 10px; spacing: 8px;
                                        Text { text: "PIXEL SEEK"; color: #ebf1f5; font-size: 12px; font-weight: 600; vertical-alignment: center; }
                                        Rectangle { horizontal-stretch: 1; }
                                        Text { text: "AVAILABLE"; color: #43cfa0; font-family: root.mono-font; font-size: 11px; vertical-alignment: center; }
                                    }
                                }
                                Rectangle { horizontal-stretch: 1; height: 34px; border-radius: 5px; border-width: 1px; border-color: #344454; background: #0f151c;
                                    HorizontalLayout { padding-left: 10px; padding-right: 10px; spacing: 8px;
                                        Text { text: "OBJECT DETECTION"; color: #9aa9b4; font-size: 12px; vertical-alignment: center; }
                                        Rectangle { horizontal-stretch: 1; }
                                        Text { text: "UNAVAILABLE"; color: #9aa9b4; font-family: root.mono-font; font-size: 11px; vertical-alignment: center; }
                                    }
                                }
                            }
                            if field.kind != "section" && field.kind != "engine": HorizontalLayout {
                                spacing: 10px;
                                Text { text: field.label; color: #c4d0d8; horizontal-stretch: 1; vertical-alignment: center; }
                                if field.kind == "text" || field.kind == "number": Rectangle {
                                    width: 250px; height: 34px; border-radius: 5px; border-width: 1px; border-color: #344454; background: #0f151c;
                                    TextInput {
                                        x: 9px; y: 2px; width: parent.width - 18px; height: parent.height - 4px;
                                        text: field.value; font-family: root.mono-font; font-size: 12px; color: #ebf1f5;
                                        edited => { root.field-edited(field.key, self.text); }
                                    }
                                }
                            if field.kind == "toggle": ConfigAction {
                                    width: 250px;
                                    caption: field.value == "true" ? "On" : "Off";
                                    active: field.value == "true";
                                    activated => { root.field-activated(field.key); }
                                }
                                if field.kind == "choice": HorizontalLayout {
                                    width: field.key == "profile.select" ? 460px : 250px; spacing: 4px;
                                    for option in field.choices: ConfigAction {
                                        horizontal-stretch: 1; caption: option.label;
                                        active: option.value == field.value;
                                        activated => { root.field-selected(field.key, option.value); }
                                    }
                                }
                                if field.kind == "profile-actions": HorizontalLayout {
                                    horizontal-stretch: 1; spacing: 8px;
                                    for action in field.choices: ConfigAction {
                                        horizontal-stretch: 1; caption: action.label;
                                        danger: action.value == "profile.delete";
                                        activated => { root.field-activated(action.value); }
                                    }
                                }
                                if field.kind == "action": ConfigAction {
                                    width: 120px; caption: field.value;
                                    activated => { root.field-activated(field.key); }
                                }
                            }
                        }
                }
            }
            HorizontalLayout {
                height: 42px; spacing: 12px;
                Text { text: root.message; color: root.message == "Changes applied successfully." ? #43cfa0 : #e8b858; vertical-alignment: center; }
                Rectangle { horizontal-stretch: 1; }
                ConfigAction {
                    width: 140px; control-height: 38px; caption: "Apply Changes"; primary: true;
                    activated => { root.apply-requested(); }
                }
            }
        }
    }
}
fn ink(rgb: u32) -> slint::Color {
    slint::Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn setting_field(
    fields: &mut Vec<ConfigField>,
    settings: &serde_json::Value,
    key: &str,
    label: &str,
    kind: &str,
) {
    let value = key
        .split('.')
        .try_fold(settings, |value, part| value.get(part))
        .map(|value| {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_i64)
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                })
        })
        .unwrap_or_default();
    let choices = if kind == "choice" {
        let items: &[(&str, &str)] = match key {
            "pixelColor" => &[("Yellow", "yellow"), ("Red", "red"), ("Purple", "purple")],
            "scale" => &[("Smooth", "Smooth"), ("Pixel perfect", "Pixel perfect")],
            "triggerButton" | "auto.triggerButton" => {
                &[("Right", "right"), ("M4", "m4"), ("M5", "m5")]
            }
            "auto.mode" => &[("Center crossing", "center"), ("Aim + fire", "aim")],
            _ => &[],
        };
        items
            .iter()
            .map(|(label, value)| ConfigOption {
                label: (*label).into(),
                value: (*value).into(),
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    fields.push(ConfigField {
        key: key.into(),
        label: label.into(),
        value: value.into(),
        kind: kind.into(),
        choices: ModelRc::new(VecModel::from(choices)),
    });
}

fn settings_fields(settings: &serde_json::Value) -> Vec<ConfigField> {
    let mut fields = Vec::new();
    let section = |fields: &mut Vec<ConfigField>, label: &str| {
        fields.push(ConfigField {
            key: "".into(),
            label: label.into(),
            value: String::new().into(),
            kind: "section".into(),
            choices: ModelRc::default(),
        });
    };
    section(&mut fields, "PROFILES");
    let profiles = settings
        .get("profiles")
        .and_then(serde_json::Value::as_array);
    let active_profile = settings
        .get("activeProfile")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let profile_options = profiles
        .into_iter()
        .flatten()
        .filter_map(|profile| profile.get("name").and_then(serde_json::Value::as_str))
        .map(|name| ConfigOption {
            label: name.into(),
            value: name.into(),
        })
        .collect::<Vec<_>>();
    fields.push(ConfigField {
        key: "profile.select".into(),
        label: "Active profile".into(),
        value: profiles
            .and_then(|items| items.get(active_profile))
            .and_then(|profile| profile.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Default")
            .into(),
        kind: "choice".into(),
        choices: ModelRc::new(VecModel::from(profile_options)),
    });
    fields.push(ConfigField {
        key: "profile.rename".into(),
        label: "Profile name".into(),
        value: profiles
            .and_then(|items| items.get(active_profile))
            .and_then(|profile| profile.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Default")
            .into(),
        kind: "text".into(),
        choices: ModelRc::default(),
    });
    fields.push(ConfigField {
        key: "profile.actions".into(),
        label: "Profile actions".into(),
        value: String::new().into(),
        kind: "profile-actions".into(),
        choices: ModelRc::new(VecModel::from(vec![
            ConfigOption {
                label: "Create".into(),
                value: "profile.create".into(),
            },
            ConfigOption {
                label: "Clone".into(),
                value: "profile.clone".into(),
            },
            ConfigOption {
                label: "Rename".into(),
                value: "profile.rename.apply".into(),
            },
            ConfigOption {
                label: "Delete".into(),
                value: "profile.delete".into(),
            },
        ])),
    });
    section(&mut fields, "DETECTION ENGINE");
    fields.push(ConfigField {
        key: "engine".into(),
        label: String::new().into(),
        value: String::new().into(),
        kind: "engine".into(),
        choices: ModelRc::default(),
    });
    section(&mut fields, "MAIN");
    setting_field(
        &mut fields,
        settings,
        "pixelColor",
        "Pixel Seek color",
        "choice",
    );
    setting_field(
        &mut fields,
        settings,
        "fovDegrees",
        "FOV (degrees)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "colorFilter",
        "Color Filter",
        "toggle",
    );
    setting_field(
        &mut fields,
        settings,
        "cropResolution",
        "Crop Resolution",
        "number",
    );
    setting_field(&mut fields, settings, "scale", "Preview scaling", "choice");
    section(&mut fields, "MOTION");
    setting_field(&mut fields, settings, "motionEnabled", "Motion", "toggle");
    setting_field(
        &mut fields,
        settings,
        "motionAlwaysOn",
        "Always on",
        "toggle",
    );
    setting_field(
        &mut fields,
        settings,
        "motionNoDeadzone",
        "No deadzone",
        "toggle",
    );
    setting_field(
        &mut fields,
        settings,
        "triggerButton",
        "Trigger button",
        "choice",
    );
    setting_field(&mut fields, settings, "offsetX", "X offset (px)", "number");
    setting_field(&mut fields, settings, "offsetY", "Y offset (px)", "number");
    setting_field(&mut fields, settings, "motionSpeedX", "X speed", "number");
    setting_field(&mut fields, settings, "motionSpeedY", "Y speed", "number");
    setting_field(
        &mut fields,
        settings,
        "smoothFovDegrees",
        "Smooth FOV (degrees)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "smoothingXMin",
        "X smoothing min",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "smoothingXMax",
        "X smoothing max",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "smoothingYMin",
        "Y smoothing min",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "smoothingYMax",
        "Y smoothing max",
        "number",
    );
    setting_field(&mut fields, settings, "sens", "Sensitivity", "number");
    setting_field(
        &mut fields,
        settings,
        "motionRenderer.enabled",
        "Normal + ABCurves",
        "toggle",
    );
    section(&mut fields, "AUTO");
    setting_field(&mut fields, settings, "auto.enabled", "Auto", "toggle");
    setting_field(&mut fields, settings, "auto.mode", "Mode", "choice");
    setting_field(
        &mut fields,
        settings,
        "auto.triggerButton",
        "Trigger button",
        "choice",
    );
    setting_field(
        &mut fields,
        settings,
        "auto.fovDegrees",
        "FOV (degrees)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "auto.delayMs",
        "Delay (ms)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "auto.cooldownMs",
        "Cooldown (ms)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "auto.sleepMs",
        "Sleep (ms)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "auto.offsetX",
        "X offset (px)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "auto.offsetY",
        "Y offset (px)",
        "number",
    );
    if settings
        .pointer("/auto/mode")
        .and_then(serde_json::Value::as_str)
        != Some("aim")
    {
        setting_field(&mut fields, settings, "auto.speedX", "X speed", "number");
        setting_field(&mut fields, settings, "auto.speedY", "Y speed", "number");
        setting_field(
            &mut fields,
            settings,
            "auto.smoothFovDegrees",
            "Smooth FOV (degrees)",
            "number",
        );
        setting_field(
            &mut fields,
            settings,
            "auto.smoothingXMin",
            "X smoothing min",
            "number",
        );
        setting_field(
            &mut fields,
            settings,
            "auto.smoothingXMax",
            "X smoothing max",
            "number",
        );
        setting_field(
            &mut fields,
            settings,
            "auto.smoothingYMin",
            "Y smoothing min",
            "number",
        );
        setting_field(
            &mut fields,
            settings,
            "auto.smoothingYMax",
            "Y smoothing max",
            "number",
        );
    }
    section(&mut fields, "RECOIL");
    setting_field(
        &mut fields,
        settings,
        "recoil.recoilEnabled",
        "Recoil",
        "toggle",
    );
    setting_field(
        &mut fields,
        settings,
        "recoil.recoilLevels",
        "Levels (comma-separated)",
        "text",
    );
    setting_field(
        &mut fields,
        settings,
        "recoil.recoilPercentage",
        "Strength (%)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "recoil.recoilStartDelayMs",
        "Start delay (ms)",
        "number",
    );
    setting_field(
        &mut fields,
        settings,
        "recoil.recoilLevelDurationMs",
        "Time per level (ms)",
        "number",
    );
    section(&mut fields, "SETTINGS");
    setting_field(&mut fields, settings, "port", "UDP input port", "number");
    fields
}

fn update_setting(settings: &mut serde_json::Value, key: &str, input: &str) -> Result<(), String> {
    let parts = key.split('.').collect::<Vec<_>>();
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| "setting key is empty".to_owned())?;
    let target = parents.iter().try_fold(settings, |value, part| {
        value
            .get_mut(*part)
            .ok_or_else(|| format!("unknown setting {key}"))
    })?;
    let current = target
        .get_mut(*leaf)
        .ok_or_else(|| format!("unknown setting {key}"))?;
    *current = if current.is_boolean() {
        serde_json::Value::Bool(
            input
                .parse()
                .map_err(|_| format!("{key} must be true or false"))?,
        )
    } else if current.is_number() {
        if current.as_u64().is_some() {
            serde_json::json!(input
                .parse::<u64>()
                .map_err(|_| format!("{key} must be a whole number"))?)
        } else if current.as_i64().is_some() {
            serde_json::json!(input
                .parse::<i64>()
                .map_err(|_| format!("{key} must be a whole number"))?)
        } else {
            let number = input
                .parse::<f64>()
                .map_err(|_| format!("{key} must be a number"))?;
            if !number.is_finite() {
                return Err(format!("{key} must be finite"));
            }
            serde_json::json!(number)
        }
    } else if current.is_array() && key == "recoil.recoilLevels" {
        serde_json::json!(sysx_ui::settings::parse_recoil_levels(input).map_err(str::to_owned)?)
    } else {
        serde_json::Value::String(input.to_owned())
    };
    Ok(())
}

fn activate_setting(settings: &mut serde_json::Value, key: &str) -> Result<(), String> {
    let current = key
        .split('.')
        .try_fold(&*settings, |value, part| value.get(part));
    let choices: &[&str] = match key {
        "pixelColor" => &["yellow", "red", "purple"],
        "scale" => &["Smooth", "Pixel perfect"],
        "triggerButton" | "auto.triggerButton" => &["right", "m4", "m5"],
        "auto.mode" => &["center", "aim"],
        "colorFilter"
        | "motionEnabled"
        | "motionAlwaysOn"
        | "motionNoDeadzone"
        | "motionRenderer.enabled"
        | "auto.enabled"
        | "recoil.recoilEnabled" => {
            let value = current
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| format!("unknown setting {key}"))?;
            return update_setting(settings, key, if value { "false" } else { "true" });
        }
        _ => return Err(format!("setting {key} is not a toggle or choice")),
    };
    let current = current
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("unknown setting {key}"))?;
    let next = choices
        .iter()
        .position(|choice| *choice == current)
        .map(|index| choices[(index + 1) % choices.len()])
        .unwrap_or(choices[0]);
    update_setting(settings, key, next)
}
#[derive(Default)]
struct RenderCache {
    uploaded: Option<Instant>,
    overlay: String,
    target: String,
}

fn refresh(app: &Receiver, view: &View, last: &RefCell<RenderCache>) {
    let state = view.state.as_ref();
    let (status, color) = view.status();
    app.set_status(status.into());
    app.set_status_ink(ink(color));
    app.set_detail(
        if !view.error.is_empty() {
            view.error.as_str()
        } else {
            state.and_then(|s| s.last_error.as_deref()).unwrap_or("")
        }
        .into(),
    );
    app.set_profile(
        state
            .and_then(|s| {
                s.profiles
                    .iter()
                    .find(|p| p.index == s.profile_index as usize)
            })
            .map(|p| p.name.as_str())
            .unwrap_or("Default")
            .into(),
    );
    let device = state.and_then(|s| s.device.as_ref());
    let (label, color) = match device.map(|d| d.state.as_str()) {
        Some("connected") => (
            device
                .and_then(|d| d.name.as_deref())
                .unwrap_or("Device connected"),
            0x43cfa0,
        ),
        Some("error") => ("Device error", 0xef6c6c),
        _ => ("No device", 0x9aa9b4),
    };
    app.set_device(label.into());
    app.set_device_ink(ink(color));
    app.set_mouse_keys(ModelRc::new(VecModel::from(
        ["Left", "Right", "Middle", "M4", "M5"]
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let held = device.and_then(|d| d.buttons.get(i)).copied().flatten();
                MouseKey {
                    label: (*label).into(),
                    known: held.is_some(),
                    down: held == Some(true),
                }
            })
            .collect::<Vec<_>>(),
    )));
    let Some(frame) = view.live_frame() else {
        app.set_has_frame(false);
        app.set_resolution("—".into());
        app.set_fps("—".into());
        app.set_latency("—".into());
        app.set_high_latency(false);
        app.set_overlay(slint::Image::default());
        app.set_has_target(false);
        last.borrow_mut().uploaded = None;
        last.borrow_mut().overlay.clear();
        last.borrow_mut().target.clear();
        return;
    };
    let received = frame.received;
    if last.borrow().uploaded != Some(received) {
        let mut rgba = crate::native_monitor::lock_recover(&frame.rgba);
        if let Some(rgba) = rgba.take() {
            app.set_preview(slint::Image::from_rgba8(rgba));
            last.borrow_mut().uploaded = Some(received);
        }
    }
    app.set_has_frame(true);
    let meta = &frame.metadata;
    app.set_resolution(
        format!(
            "{} × {}",
            meta.detection.processed_width, meta.detection.processed_height
        )
        .into(),
    );
    app.set_fps(
        meta.processing_fps
            .filter(|v| v.is_finite())
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "—".into())
            .into(),
    );
    app.set_latency(format!("{:.1}", meta.detection.total_processing_ms).into());
    let tone = |name| {
        ink(match name {
            Some("live") => 0x43cfa0,
            Some("wait") => 0xe8b858,
            Some("error") => 0xef6c6c,
            _ => 0xebf1f5,
        })
    };
    app.set_fps_ink(tone(sysx_ui::layout::fps_tone(
        meta.processing_fps,
        meta.source_fps,
    )));
    app.set_latency_ink(tone(sysx_ui::layout::latency_tone(
        meta.detection.total_processing_ms,
    )));
    app.set_high_latency(meta.detection.total_processing_ms > sysx_ui::layout::LATENCY_HIGH_MS);
    let pixel = state.is_some_and(|s| s.preview_scale == "Pixel perfect");
    app.set_pixel_perfect(pixel);
    let side = app.get_feed_size() as f64;
    let Some(mut rect) = fit_preview_image(
        meta.source_width as f64,
        meta.source_height as f64,
        side - 2.,
        side - 2.,
        pixel,
    ) else {
        return;
    };
    rect.x += 1.;
    rect.y += 1.;
    app.set_image_x(rect.x as f32);
    app.set_image_y(rect.y as f32);
    app.set_image_width(rect.width as f32);
    app.set_image_height(rect.height as f32);
    let fov = if meta.detection.fov_degrees > 0 {
        meta.detection.fov_degrees
    } else {
        state.map(|s| s.fov_degrees).unwrap_or(36)
    };
    let d = fov_diameter(rect, (meta.source_width, meta.source_height), fov).unwrap_or(0.);
    app.set_fov_size(d as f32);
    app.set_fov_x((rect.x + (rect.width - d) / 2.) as f32);
    app.set_fov_y((rect.y + (rect.height - d) / 2.) as f32);
    app.set_fov_ink(ink(if meta.detection.auto_active {
        0x4da7e8
    } else {
        0x43cfa0
    }));
    let color = state
        .map(|s| {
            if s.effective_pixel_color.is_empty() {
                s.pixel_color.as_str()
            } else {
                s.effective_pixel_color.as_str()
            }
        })
        .unwrap_or("yellow");
    let color = ink(match color {
        "red" => 0xe26868,
        "purple" => 0xa682dc,
        _ => 0xe3bf4d,
    });
    app.set_target_ink(color);
    let overlay = overlay_svg(meta, rect, side, target_hex(color_name(state)));
    if last.borrow().overlay != overlay {
        if let Ok(image) = slint::Image::load_from_svg_data(overlay.as_bytes()) {
            app.set_overlay(image);
            last.borrow_mut().overlay = overlay;
        }
    }
    // The line and ring move every frame while there is a target. They are
    // rasterized into a small image around them: re-rasterizing and
    // uploading the whole full-size overlay at the frame rate made weak
    // machines stutter while Motion tracked.
    let layer = target_layer(meta, rect, target_hex(color_name(state)));
    if let Some(layer) = &layer {
        if last.borrow().target != layer.svg {
            if let Ok(image) = slint::Image::load_from_svg_data(layer.svg.as_bytes()) {
                app.set_target_layer(image);
                last.borrow_mut().target = layer.svg.clone();
            }
        }
        app.set_target_x(layer.x as f32);
        app.set_target_y(layer.y as f32);
        app.set_target_w(layer.width as f32);
        app.set_target_h(layer.height as f32);
    }
    app.set_detection_summary(
        if meta.mode == "object_detection" {
            sysx_ui::layout::detection_summary(&meta.detection.objects)
        } else {
            String::new()
        }
        .into(),
    );
    app.set_has_target(layer.is_some());
}

static REFRESH_PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

fn catch_refresh(run: impl FnOnce()) -> Option<String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .err()
        .map(|panic| {
            panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic")
                .to_owned()
        })
}

fn refresh_guarded(app: &Receiver, view: &View, last: &RefCell<RenderCache>) {
    if let Some(message) = catch_refresh(|| refresh(app, view, last)) {
        if !REFRESH_PANIC_REPORTED.swap(true, Ordering::AcqRel) {
            crate::diagnostics::record(&format!(
                "Native receiver UI recovered from refresh panic: {message}"
            ));
        }
    }
}

/// The ring marks the pixel being targeted; Object Detection keeps its
/// smoothed point.
fn target_point(meta: &sysx_ui::protocol::LiveFrame) -> Option<[f32; 2]> {
    if meta.mode == "object_detection" {
        meta.detection
            .smoothed_target_xy
            .or(meta.detection.target_xy)
    } else {
        meta.detection.target_xy
    }
}

fn color_name(state: Option<&sysx_ui::protocol::BackendState>) -> &str {
    state
        .map(|s| {
            if s.effective_pixel_color.is_empty() {
                s.pixel_color.as_str()
            } else {
                s.effective_pixel_color.as_str()
            }
        })
        .unwrap_or("yellow")
}
/// Native SVG image primitives preserve the old corner/reticle geometry on
/// both Slint's GPU and software renderers (software Path items are unsupported).
/// This full-size layer holds only Object Detection boxes; the target line and
/// ring are a separate small layer (`target_layer`).
fn overlay_svg(
    meta: &sysx_ui::protocol::LiveFrame,
    rect: sysx_ui::layout::Rect,
    side: f64,
    color: &str,
) -> String {
    use std::fmt::Write;
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{side}" height="{side}" viewBox="0 0 {side} {side}">"#
    );
    let (pw, ph, cx, cy) = (
        meta.detection.processed_width as f64,
        meta.detection.processed_height as f64,
        meta.crop_x as f64,
        meta.crop_y as f64,
    );
    // Pixel Seek draws only the line and ring to its one target pixel (user
    // request); boxes are for Object Detection.
    let boxes = if meta.mode == "object_detection" {
        meta.detection.objects.as_slice()
    } else {
        &[]
    };
    for object in boxes {
        let Some(b) = map_detection_box(object.bbox_xyxy.map(f64::from), pw, ph, rect, cx, cy)
        else {
            continue;
        };
        if b.width <= 0. || b.height <= 0. {
            continue;
        }
        let stroke = if meta.mode == "object_detection" {
            format!(
                "hsl({},82%,62%)",
                (object.class_id as f64 * 137.508 + 24.) % 360.
            )
        } else {
            color.to_owned()
        };
        let (x, y, w, h) = (b.x, b.y, b.width, b.height);
        let c = (w.min(h) * 0.22).clamp(7., 16.);
        let _ = write!(
            svg,
            r##"<g fill="none" stroke-linejoin="round"><rect x="{x}" y="{y}" width="{w}" height="{h}" stroke="#05090a" stroke-opacity=".88" stroke-width="4"/><rect x="{x}" y="{y}" width="{w}" height="{h}" stroke="{stroke}" stroke-width="1.5"/><path transform="translate({x},{y})" d="M0 {c}V0H{c} M{} 0H{w}V{c} M{w} {}V{h}H{} M{c} {h}H0V{}" stroke="{stroke}" stroke-width="3"/></g>"##,
            w - c,
            h - c,
            w - c,
            h - c
        );
    }
    svg.push_str("</svg>");
    svg
}

fn target_hex(color: &str) -> &'static str {
    match color {
        "red" => "#e26868",
        "purple" => "#a682dc",
        _ => "#e3bf4d",
    }
}

/// The line from the center to the target and the ring on it, as an SVG
/// covering only their bounding box (preview coordinates).
struct TargetLayer {
    svg: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Reach of the ring, its ticks and their outline around the target point.
const TARGET_LAYER_PAD: f64 = 20.;

fn target_layer(
    meta: &sysx_ui::protocol::LiveFrame,
    rect: sysx_ui::layout::Rect,
    color: &str,
) -> Option<TargetLayer> {
    use std::fmt::Write;
    // Center-crossing Auto has no target; its aim mode does.
    if meta.detection.auto_active && !meta.detection.auto_aim {
        return None;
    }
    let (pw, ph, cx, cy) = (
        meta.detection.processed_width as f64,
        meta.detection.processed_height as f64,
        meta.crop_x as f64,
        meta.crop_y as f64,
    );
    let t = target_point(meta)
        .and_then(|t| map_detection_point(t.map(f64::from), pw, ph, rect, cx, cy))?;
    let center = [rect.x + rect.width / 2., rect.y + rect.height / 2.];
    let x0 = (t[0].min(center[0]) - TARGET_LAYER_PAD).floor();
    let y0 = (t[1].min(center[1]) - TARGET_LAYER_PAD).floor();
    let width = (t[0].max(center[0]) + TARGET_LAYER_PAD).ceil() - x0;
    let height = (t[1].max(center[1]) + TARGET_LAYER_PAD).ceil() - y0;
    // The viewBox starts at the layer's origin, so the drawing keeps preview
    // coordinates.
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="{x0} {y0} {width} {height}">"#
    );
    {
        let (x, y) = (t[0], t[1]);
        let distance = (x - center[0]).hypot(y - center[1]);
        if distance > 12. {
            let end = [
                x - (x - center[0]) / distance * 12.,
                y - (y - center[1]) / distance * 12.,
            ];
            let _ = write!(
                svg,
                r##"<defs><linearGradient id="lead" gradientUnits="userSpaceOnUse" x1="{}" y1="{}" x2="{}" y2="{}"><stop stop-color="{color}" stop-opacity=".1"/><stop offset="1" stop-color="{color}" stop-opacity=".95"/></linearGradient></defs><path d="M{} {}L{} {}" stroke="#040708" stroke-opacity=".85" stroke-width="3"/><path d="M{} {}L{} {}" stroke="url(#lead)" stroke-width="1.25"/>"##,
                center[0],
                center[1],
                end[0],
                end[1],
                center[0],
                center[1],
                end[0],
                end[1],
                center[0],
                center[1],
                end[0],
                end[1]
            );
        }
        for (width, stroke, opacity) in [(3.5, "#040708", 0.85), (1.5, color, 1.)] {
            let _ = write!(
                svg,
                r#"<g fill="none" stroke="{stroke}" stroke-opacity="{opacity}" stroke-width="{width}" stroke-linecap="round"><circle cx="{x}" cy="{y}" r="8"/>"#
            );
            for (ux, uy) in [(0., -1.), (0., 1.), (-1., 0.), (1., 0.)] {
                let _ = write!(
                    svg,
                    r#"<path d="M{} {}L{} {}"/>"#,
                    x + ux * 11.,
                    y + uy * 11.,
                    x + ux * 16.,
                    y + uy * 16.
                );
            }
            svg.push_str("</g>");
        }
        let _ = write!(
            svg,
            r#"<circle cx="{x}" cy="{y}" r="1.6" fill="{color}"/><circle cx="{}" cy="{}" r="2" fill="{color}" fill-opacity=".7"/>"#,
            center[0], center[1]
        );
    }
    svg.push_str("</svg>");
    Some(TargetLayer {
        svg,
        x: x0,
        y: y0,
        width,
        height,
    })
}

pub fn run(
    port: u16,
    token: String,
    events: EventReceiver<crate::native_app::HostEvent>,
    commands: std::sync::mpsc::SyncSender<serde_json::Value>,
) -> Result<(), String> {
    let app = Receiver::new().map_err(|e| e.to_string())?;
    let settings_app = SettingsWindow::new().map_err(|e| e.to_string())?;
    let view = Arc::new(Mutex::new(View::default()));
    let queued = Arc::new(AtomicBool::new(false));
    // Coalesce UI wake-ups; workers keep at most one pending JPEG and one decoded frame.
    let (weak, display, dirty) = (app.as_weak(), Arc::clone(&view), Arc::clone(&queued));
    let update = Rc::new(move |app: &Receiver| {
        dirty.store(false, Ordering::Release);
        let snapshot = display.lock().map(|v| v.clone()).unwrap_or_default();
        UPLOAD.with(|last| refresh_guarded(app, &snapshot, last));
    });
    // UI closures live on the event-loop thread; worker notifications carry only a weak handle.
    let ui_update = Rc::clone(&update);
    // Let the decoder go on once a frame is on screen. Renderers without a
    // rendering notifier (software) release it when the frame is uploaded.
    let awaiting = view
        .lock()
        .map(|view| Arc::clone(&view.awaiting_draw))
        .unwrap_or_default();
    let drawn = Arc::clone(&awaiting);
    let notified = app
        .window()
        .set_rendering_notifier(move |state, _| {
            if matches!(state, slint::RenderingState::AfterRendering) {
                drawn.mark_drawn();
            }
        })
        .is_ok();
    let release_on_upload = (!notified).then_some(awaiting);
    let display = Arc::clone(&view);
    let dirty = Arc::clone(&queued);
    let notify = Arc::new(move || {
        if !dirty.swap(true, Ordering::AcqRel) {
            let view = Arc::clone(&display);
            let dirty = Arc::clone(&dirty);
            let release_on_upload = release_on_upload.clone();
            let _ = weak.upgrade_in_event_loop(move |app| {
                dirty.store(false, Ordering::Release);
                let snapshot = view.lock().map(|v| v.clone()).unwrap_or_default();
                UPLOAD.with(|last| refresh_guarded(&app, &snapshot, last));
                if let Some(awaiting) = &release_on_upload {
                    awaiting.mark_drawn();
                }
            });
        }
    });
    let workers = Rc::new(RefCell::new(Some(Workers::start(
        port,
        token.clone(),
        Arc::clone(&view),
        notify.clone(),
    )?)));
    let lifecycle = Rc::new(RefCell::new(None::<crate::native_app::MonitorLifecycle>));
    let hidden = Rc::new(Cell::new(false));
    let settings_visible = Rc::new(Cell::new(false));
    let settings_draft = Rc::new(RefCell::new(None::<serde_json::Value>));
    let settings_dirty = Rc::new(Cell::new(false));
    let invalid_settings = Rc::new(RefCell::new(HashSet::<String>::new()));
    let profile_name = Rc::new(RefCell::new(String::new()));
    let settings_weak = settings_app.as_weak();
    app.on_settings_requested({
        let visible = Rc::clone(&settings_visible);
        let dirty = Rc::clone(&settings_dirty);
        let commands = commands.clone();
        move || {
            let was_visible = visible.replace(true);
            if let Some(settings) = settings_weak.upgrade() {
                if !was_visible && !dirty.get() {
                    settings.set_message("Loading settings…".into());
                    let _ = commands.try_send(serde_json::json!({
                        "version":1,"type":"get_app_settings"
                    }));
                }
                let _ = settings.show();
                settings.invoke_reset_scroll();
            }
        }
    });
    settings_app.window().on_close_requested({
        let visible = Rc::clone(&settings_visible);
        move || {
            visible.set(false);
            slint::CloseRequestResponse::HideWindow
        }
    });
    settings_app.on_close_requested({
        let visible = Rc::clone(&settings_visible);
        let weak = settings_app.as_weak();
        move || {
            visible.set(false);
            if let Some(settings) = weak.upgrade() {
                settings.hide().ok();
            }
        }
    });
    settings_app.on_field_edited({
        let draft = Rc::clone(&settings_draft);
        let dirty = Rc::clone(&settings_dirty);
        let invalid = Rc::clone(&invalid_settings);
        let profile_name = Rc::clone(&profile_name);
        let weak = settings_app.as_weak();
        move |key, value| {
            let result = if key == "profile.rename" {
                *profile_name.borrow_mut() = value.to_string();
                Ok(())
            } else {
                let result = draft
                    .borrow_mut()
                    .as_mut()
                    .ok_or_else(|| "Settings have not loaded yet".to_owned())
                    .and_then(|settings| update_setting(settings, &key, &value));
                dirty.set(true);
                if result.is_ok() {
                    invalid.borrow_mut().remove(key.as_str());
                } else {
                    invalid.borrow_mut().insert(key.to_string());
                }
                result
            };
            if let Some(app) = weak.upgrade() {
                app.set_message(result.err().unwrap_or_default().into());
            }
        }
    });
    settings_app.on_field_selected({
        let draft = Rc::clone(&settings_draft);
        let dirty = Rc::clone(&settings_dirty);
        let invalid = Rc::clone(&invalid_settings);
        let commands = commands.clone();
        let weak = settings_app.as_weak();
        move |key, value| {
            let result = if key == "profile.select" {
                if dirty.get() {
                    Err("Apply current settings before changing profiles.".to_owned())
                } else {
                    let index = draft
                        .borrow()
                        .as_ref()
                        .and_then(|settings| settings.get("profiles")?.as_array())
                        .and_then(|profiles| {
                            profiles.iter().position(|profile| {
                                profile.get("name").and_then(serde_json::Value::as_str)
                                    == Some(value.as_str())
                            })
                        })
                        .ok_or_else(|| "Profile is no longer available.".to_owned());
                    index.and_then(|index| {
                        commands
                            .try_send(serde_json::json!({
                                "version":1,"type":"activate_profile","index":index
                            }))
                            .map_err(|_| "Backend command queue is unavailable.".to_owned())
                    })
                }
            } else {
                let result = draft
                    .borrow_mut()
                    .as_mut()
                    .ok_or_else(|| "Settings have not loaded yet".to_owned())
                    .and_then(|settings| update_setting(settings, &key, &value));
                dirty.set(true);
                if result.is_ok() {
                    invalid.borrow_mut().remove(key.as_str());
                }
                result
            };
            if let Some(app) = weak.upgrade() {
                if key != "profile.select" {
                    if let Some(settings) = draft.borrow().as_ref() {
                        app.set_fields(ModelRc::new(VecModel::from(settings_fields(settings))));
                    }
                }
                app.set_message(if key == "profile.select" {
                    result
                        .map(|_| "Updating profile…".to_owned())
                        .unwrap_or_else(|error| error)
                        .into()
                } else {
                    result.err().unwrap_or_default().into()
                });
            }
        }
    });
    settings_app.on_field_activated({
        let draft = Rc::clone(&settings_draft);
        let dirty = Rc::clone(&settings_dirty);
        let invalid = Rc::clone(&invalid_settings);
        let profile_name = Rc::clone(&profile_name);
        let commands = commands.clone();
        let weak = settings_app.as_weak();
        move |key| {
            if key.starts_with("profile.") {
                let result = if dirty.get() {
                    Err("Apply current settings before changing profiles.".to_owned())
                } else {
                    let draft = draft.borrow();
                    let settings = draft.as_ref();
                    let command = match key.as_str() {
                        "profile.select" => settings
                            .and_then(|settings| {
                                let count = settings.get("profiles")?.as_array()?.len();
                                let active = settings.get("activeProfile")?.as_u64()? as usize;
                                (count > 0).then(|| serde_json::json!({
                                    "version":1,"type":"activate_profile","index":(active + 1) % count
                                }))
                            })
                            .ok_or_else(|| "Profiles are not available.".to_owned()),
                        "profile.create" => Ok(serde_json::json!({
                            "version":1,"type":"create_profile","duplicate":false
                        })),
                        "profile.clone" => Ok(serde_json::json!({
                            "version":1,"type":"create_profile","duplicate":true
                        })),
                        "profile.delete" => Ok(serde_json::json!({
                            "version":1,"type":"delete_profile"
                        })),
                        "profile.rename.apply" => Ok(serde_json::json!({
                            "version":1,"type":"rename_profile","name":profile_name.borrow().trim()
                        })),
                        _ => Err(format!("Unknown profile action: {key}")),
                    };
                    command.and_then(|command| {
                        commands
                            .try_send(command)
                            .map_err(|_| "Backend command queue is unavailable.".to_owned())
                    })
                };
                if let Some(app) = weak.upgrade() {
                    app.set_message(result.map(|_| "Updating profile…".to_owned()).unwrap_or_else(|error| error).into());
                }
                return;
            }
            let result = draft
                .borrow_mut()
                .as_mut()
                .ok_or_else(|| "Settings have not loaded yet".to_owned())
                .and_then(|settings| {
                    if let Some(key) = invalid.borrow().iter().next() {
                        Err(format!("Fix the invalid value for {key} before applying."))
                    } else {
                        activate_setting(settings, &key)
                    }
                });
            if result.is_ok() {
                dirty.set(true);
                invalid.borrow_mut().remove(key.as_str());
            }
            if let Some(app) = weak.upgrade() {
                match result {
                    Ok(()) => {
                        if let Some(settings) = draft.borrow().as_ref() {
                            app.set_fields(ModelRc::new(VecModel::from(settings_fields(settings))));
                        }
                        app.set_message("".into());
                    }
                    Err(error) => app.set_message(error.into()),
                }
            }
        }
    });
    settings_app.on_apply_requested({
        let draft = Rc::clone(&settings_draft);
        let invalid = Rc::clone(&invalid_settings);
        let commands = commands.clone();
        let weak = settings_app.as_weak();
        move || {
            let result = draft
                .borrow()
                .clone()
                .ok_or_else(|| "Settings have not loaded yet".to_owned())
                .and_then(|settings| {
                    if let Some(key) = invalid.borrow().iter().next() {
                        Err(format!("Fix the invalid value for {key} before applying."))
                    } else {
                        Ok(settings)
                    }
                })
                .and_then(|settings| {
                    commands
                        .try_send(serde_json::json!({
                            "version":1,
                            "type":"apply_app_settings",
                            "settings":settings,
                            "request_id":"native-settings"
                        }))
                        .map_err(|_| "Backend command queue is unavailable".to_owned())
                });
            if let Some(app) = weak.upgrade() {
                app.set_message(result.err().unwrap_or_else(|| "Applying…".into()).into());
            }
        }
    });
    app.window().on_close_requested({
        let workers = Rc::clone(&workers);
        let lifecycle = Rc::clone(&lifecycle);
        let hidden = Rc::clone(&hidden);
        move || {
            workers.borrow_mut().take();
            hidden.set(true);
            *lifecycle.borrow_mut() = Some(
                crate::native_app::MonitorLifecycle::after_monitor_closed(Instant::now()),
            );
            slint::CloseRequestResponse::HideWindow
        }
    });
    app.on_viewport_changed({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                ui_update(&app);
            }
        }
    });
    update(&app);
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    let signal = crate::single_instance::MonitorSignal::new();
    let mut monitor_clients = 0;
    let settings_draft_events = Rc::clone(&settings_draft);
    let settings_dirty_events = Rc::clone(&settings_dirty);
    let invalid_settings_events = Rc::clone(&invalid_settings);
    let profile_name_events = Rc::clone(&profile_name);
    let settings_visible_events = Rc::clone(&settings_visible);
    let settings_weak_events = settings_app.as_weak();
    let settings_commands_events = commands.clone();
    let mut apply_success_until = None::<Instant>;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            loop {
                match events.try_recv() {
                    Ok(crate::native_app::HostEvent::ClientCount(count)) => monitor_clients = count,
                    Ok(crate::native_app::HostEvent::CommandResponse(value)) => {
                        let event = value.get("event").and_then(|event| event.as_str());
                        let command_type = value.get("type").and_then(|value| value.as_str());
                        if event == Some("app_settings") && !settings_dirty_events.get() {
                            apply_success_until = None;
                            if let Some(settings) = value.get("settings") {
                                *settings_draft_events.borrow_mut() = Some(settings.clone());
                                settings_dirty_events.set(false);
                                invalid_settings_events.borrow_mut().clear();
                                *profile_name_events.borrow_mut() = settings
                                    .get("profiles")
                                    .and_then(serde_json::Value::as_array)
                                    .and_then(|profiles| {
                                        settings
                                            .get("activeProfile")
                                            .and_then(serde_json::Value::as_u64)
                                            .and_then(|index| profiles.get(index as usize))
                                    })
                                    .and_then(|profile| profile.get("name"))
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("Default")
                                    .to_owned();
                                if let Some(app) = settings_weak_events.upgrade() {
                                    app.set_fields(ModelRc::new(VecModel::from(settings_fields(
                                        settings,
                                    ))));
                                    app.set_message("".into());
                                }
                            }
                        } else if command_type == Some("apply_app_settings") {
                            if let Some(app) = settings_weak_events.upgrade() {
                                if event == Some("ack") {
                                    settings_dirty_events.set(false);
                                    invalid_settings_events.borrow_mut().clear();
                                    apply_success_until =
                                        Some(Instant::now() + Duration::from_secs(3));
                                    app.set_message("Changes applied successfully.".into());
                                } else {
                                    apply_success_until = None;
                                    app.set_message(
                                        value
                                            .get("message")
                                            .and_then(|message| message.as_str())
                                            .unwrap_or("Settings were not applied.")
                                            .into(),
                                    );
                                }
                            }
                        } else if event == Some("ack")
                            && matches!(
                                command_type,
                                Some(
                                    "activate_profile"
                                        | "create_profile"
                                        | "rename_profile"
                                        | "delete_profile"
                                )
                            )
                        {
                            let _ = settings_commands_events.try_send(serde_json::json!({
                                "version":1,"type":"get_app_settings"
                            }));
                        } else if event == Some("error")
                            && command_type.is_some_and(|kind| kind.ends_with("profile"))
                        {
                            if let Some(app) = settings_weak_events.upgrade() {
                                app.set_message(
                                    value
                                        .get("message")
                                        .and_then(|message| message.as_str())
                                        .unwrap_or("Profile action failed.")
                                        .into(),
                                );
                            }
                        } else if event == Some("error") {
                            crate::diagnostics::record(&format!(
                                "Native command failed: {}",
                                value
                                    .get("message")
                                    .and_then(|message| message.as_str())
                                    .unwrap_or("unknown error")
                            ));
                        }
                    }
                    Ok(crate::native_app::HostEvent::Disconnected)
                    | Err(TryRecvError::Disconnected) => {
                        let _ = slint::quit_event_loop();
                        return;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            if apply_success_until.is_some_and(|deadline| Instant::now() >= deadline) {
                apply_success_until = None;
                if let Some(settings_app) = settings_weak_events.upgrade() {
                    if settings_app.get_message().as_str() == "Changes applied successfully." {
                        settings_app.set_message("".into());
                    }
                }
            }
            if signal.requested() {
                if let Some(app) = weak.upgrade() {
                    if hidden.replace(false) {
                        match Workers::start(port, token.clone(), Arc::clone(&view), notify.clone())
                        {
                            Ok(next) => *workers.borrow_mut() = Some(next),
                            Err(error) => crate::diagnostics::record(&format!(
                                "Native receiver reconnect failed: {error}"
                            )),
                        }
                    }
                    *lifecycle.borrow_mut() = None;
                    let _ = app.show();
                    app.window().request_redraw();
                }
            }
            let native_clients = monitor_clients + usize::from(settings_visible_events.get());
            if lifecycle
                .borrow_mut()
                .as_mut()
                .is_some_and(|life| life.should_exit(Some(native_clients), Instant::now()))
            {
                let _ = slint::quit_event_loop();
                return;
            }
            if !hidden.get() {
                if let Some(app) = weak.upgrade() {
                    update(&app);
                }
            }
        },
    );
    app.show().map_err(|e| e.to_string())?;
    slint::run_event_loop_until_quit().map_err(|e| e.to_string())
}
thread_local! {static UPLOAD:RefCell<RenderCache>=const{RefCell::new(RenderCache{uploaded:None,overlay:String::new(),target:String::new()})};}

#[cfg(test)]
mod overlay_routing_tests {
    use slint::Model;

    #[test]
    fn native_refresh_panics_are_caught_for_event_loop_recovery() {
        let error = super::catch_refresh(|| panic!("simulated native refresh panic"));
        assert!(error.is_some_and(|message| message.contains("simulated native refresh panic")));
        assert!(super::catch_refresh(|| {}).is_none());
    }

    #[test]
    fn native_settings_marks_only_pixel_seek_as_available() {
        let fields = super::settings_fields(&serde_json::json!({}));
        let engine = fields.iter().find(|field| field.key == "engine").unwrap();
        assert_eq!(engine.kind, "engine");
        let sections = fields
            .iter()
            .filter(|field| field.kind == "section")
            .map(|field| field.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            sections,
            [
                "PROFILES",
                "DETECTION ENGINE",
                "MAIN",
                "MOTION",
                "AUTO",
                "RECOIL",
                "SETTINGS"
            ]
        );
    }

    #[test]
    fn native_settings_show_all_choice_options_and_active_profile() {
        let fields = super::settings_fields(&serde_json::json!({
            "profiles":[{"name":"Default"},{"name":"Low recoil"}],
            "activeProfile":0,
            "pixelColor":"red",
            "scale":"Smooth",
            "triggerButton":"right",
            "auto":{"mode":"center","triggerButton":"m4"}
        }));
        let field = |key| fields.iter().find(|field| field.key == key).unwrap();
        let colors = field("pixelColor")
            .choices
            .iter()
            .map(|choice| choice.label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(colors, ["Yellow", "Red", "Purple"]);
        assert_eq!(field("pixelColor").value.as_str(), "red");
        let profiles = field("profile.select")
            .choices
            .iter()
            .map(|choice| choice.value.to_string())
            .collect::<Vec<_>>();
        assert_eq!(profiles, ["Default", "Low recoil"]);
        assert_eq!(field("profile.actions").choices.row_count(), 4);
    }

    #[test]
    fn native_aim_settings_hide_unused_auto_motion_fields() {
        let mut settings = serde_json::json!({
            "auto": {
                "mode": "aim",
                "speedX": 0.5,
                "speedY": 0.5,
                "smoothFovDegrees": 20,
                "smoothingXMin": 1,
                "smoothingXMax": 2,
                "smoothingYMin": 1,
                "smoothingYMax": 2,
                "offsetX": 0,
                "offsetY": 0
            }
        });
        let fields = super::settings_fields(&settings);
        assert!(fields.iter().any(|field| field.key == "auto.offsetX"));
        assert!(!fields.iter().any(|field| field.key == "auto.speedX"));
        assert!(!fields.iter().any(|field| field.key == "auto.smoothingXMin"));

        settings["auto"]["mode"] = serde_json::json!("center");
        let fields = super::settings_fields(&settings);
        assert!(fields.iter().any(|field| field.key == "auto.speedX"));
        assert!(fields.iter().any(|field| field.key == "auto.smoothingXMin"));
    }

    #[test]
    fn auto_hides_reticle_through_center_loss_and_pending_delay() {
        let mut meta: sysx_ui::protocol::LiveFrame = serde_json::from_value(serde_json::json!({
            "frame_id":1, "source":"test", "source_width":64, "source_height":64,
            "crop_x":0, "crop_y":0, "mode":"pixel_seek", "model_id":null,
            "processing_fps":null, "source_fps":null, "dropped_input_frames":0,
            "transport_dropped_frames":0,
            "detection":{"frame_id":1,"timestamp_ns":0,"mode":"pixel_seek",
                "model_id":null,"source_width":64,"source_height":64,
                "processed_width":64,"processed_height":64,"objects":[],
                "total_processing_ms":0,"target_xy":[48,32],"auto_active":true}
        }))
        .unwrap();
        let rect = sysx_ui::layout::Rect {
            x: 0.,
            y: 0.,
            width: 640.,
            height: 640.,
        };
        for valid in [false, true, false] {
            meta.detection.auto_target_valid = valid;
            assert!(super::target_layer(&meta, rect, "red").is_none());
        }
        meta.detection.auto_active = false;
        assert!(super::target_layer(&meta, rect, "red")
            .unwrap()
            .svg
            .contains("id=\"lead\""));
    }

    #[test]
    fn pixel_seek_draws_only_the_line_and_ring_to_its_target_pixel() {
        let mut meta: sysx_ui::protocol::LiveFrame = serde_json::from_value(serde_json::json!({
            "frame_id":1, "source":"test", "source_width":64, "source_height":64,
            "crop_x":0, "crop_y":0, "mode":"pixel_seek", "model_id":null,
            "processing_fps":null, "source_fps":null, "dropped_input_frames":0,
            "transport_dropped_frames":0,
            "detection":{"frame_id":1,"timestamp_ns":0,"mode":"pixel_seek",
                "model_id":null,"source_width":64,"source_height":64,
                "processed_width":64,"processed_height":64,
                "objects":[{"class_id":0,"class_name":"target","confidence":null,
                    "bbox_xyxy":[40,28,56,40]}],
                "total_processing_ms":0,"target_xy":[48,28],"smoothed_target_xy":[40,30]}
        }))
        .unwrap();
        let rect = sysx_ui::layout::Rect {
            x: 0.,
            y: 0.,
            width: 640.,
            height: 640.,
        };
        let svg = super::overlay_svg(&meta, rect, 640., "red");
        assert!(!svg.contains("<rect"), "no box: {svg}");
        // The ring sits on the targeted pixel, not the smoothed point, in a
        // small layer covering only the line and ring.
        let layer = super::target_layer(&meta, rect, "red").unwrap();
        assert!(layer.svg.contains("cx=\"480\" cy=\"280\""), "{}", layer.svg);
        assert_eq!(
            (layer.x, layer.y, layer.width, layer.height),
            (300., 260., 200., 80.),
            "center (320, 320) to target (480, 280), 20 px around"
        );
        assert!(
            layer.svg.contains("viewBox=\"300 260 200 80\""),
            "{}",
            layer.svg
        );
        meta.mode = "object_detection".into();
        assert!(super::overlay_svg(&meta, rect, 640., "red").contains("<rect"));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    struct Headless {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl slint::platform::Platform for Headless {
        fn create_window_adapter(
            &self,
        ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }
    #[test]
    fn native_receiver_renders_wait_live_and_narrow_without_a_browser() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Headless {
            window: window.clone(),
        }))
        .unwrap();
        let app = Receiver::new().unwrap();
        if !cfg!(windows) {
            app.set_body_font("DejaVu Sans".into());
            app.set_mono_font("DejaVu Sans Mono".into());
        }
        let mut view = View {
            connected: true,
            ..Default::default()
        };
        let last = RefCell::new(RenderCache::default());
        for (name, w, h, live) in [
            ("waiting", 550, 620, false),
            ("live", 550, 620, true),
            ("narrow", 360, 420, true),
        ] {
            window.set_size(slint::PhysicalSize::new(w, h));
            app.show().unwrap();
            if live {
                view.state=Some(serde_json::from_value(serde_json::json!({"status":"streaming","fov_degrees":90,"pixel_color":"red","profiles":[{"index":0,"name":"Default"}],"device":{"state":"connected","name":"Medius","buttons":[false,true,false,false,false]}})).unwrap());
                view.frame = Some(Arc::new(
                    crate::native_monitor::decode(crate::native_monitor::PendingFrame {
                        metadata: crate::native_monitor::sample_metadata(),
                        payload: include_bytes!("../../src/assets/red-cube-192.jpg").to_vec(),
                        payload_start: 0,
                        mask_bytes: 0,
                        received: Instant::now(),
                    })
                    .unwrap(),
                ));
            }
            refresh(&app, &view, &last);
            let mut pixels = vec![slint::Rgb8Pixel::default(); (w * h) as usize];
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, w as usize);
            });
            assert!(pixels.iter().any(|p| p.r != 0 || p.g != 0 || p.b != 0));
            assert_eq!(app.get_has_frame(), live);
            assert!(app.get_feed_size() > 200. && app.get_feed_size() <= 640.);
            if live {
                assert_eq!(app.get_device().as_str(), "Medius");
                assert!(app.get_has_target());
            }
            if let Ok(dir) = std::env::var("SYSX_NATIVE_CAPTURE_DIR") {
                std::fs::create_dir_all(&dir).unwrap();
                let raw = pixels
                    .iter()
                    .flat_map(|p| [p.r, p.g, p.b])
                    .collect::<Vec<_>>();
                image::RgbImage::from_raw(w, h, raw)
                    .unwrap()
                    .save(std::path::Path::new(&dir).join(format!("receiver-native-{name}.png")))
                    .unwrap();
            }
        }
        app.hide().unwrap();
        drop(app);

        window.set_size(slint::PhysicalSize::new(760, 720));
        let settings = SettingsWindow::new().unwrap();
        if !cfg!(windows) {
            settings.set_body_font("DejaVu Sans".into());
            settings.set_mono_font("DejaVu Sans Mono".into());
        }
        settings.set_fields(ModelRc::new(VecModel::from(settings_fields(
            &serde_json::json!({
                "profiles":[{"name":"Default"},{"name":"Low recoil"}],"activeProfile":0,
                "pixelColor":"yellow","fovDegrees":90,"colorFilter":false,
                "cropResolution":256,"scale":"Smooth","motionEnabled":true,
                "motionAlwaysOn":false,"motionNoDeadzone":true,"triggerButton":"right",
                "offsetX":0,"offsetY":0,"motionSpeedX":0.045,"motionSpeedY":0.045,
                "smoothFovDegrees":20,"smoothingXMin":1,"smoothingXMax":3,
                "smoothingYMin":1,"smoothingYMax":3,"sens":1.0,
                "motionRenderer":{"enabled":false},
                "auto":{"enabled":false,"mode":"center","triggerButton":"m4",
                    "fovDegrees":45,"delayMs":50,"cooldownMs":100,"sleepMs":0,
                    "offsetX":0,"offsetY":0,"speedX":0.045,"speedY":0.045,
                    "smoothFovDegrees":20,"smoothingXMin":1,"smoothingXMax":3,
                    "smoothingYMin":1,"smoothingYMax":3},
                "recoil":{"recoilEnabled":false,"recoilLevels":[1,2,3],
                    "recoilPercentage":50,"recoilStartDelayMs":100,"recoilLevelDurationMs":50},
                "port":4200
            }),
        ))));
        settings.show().unwrap();
        settings.invoke_reset_scroll();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 760 * 720];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 760);
        });
        assert!(pixels.iter().any(|p| p.r != 0 || p.g != 0 || p.b != 0));
        if let Ok(dir) = std::env::var("SYSX_NATIVE_CAPTURE_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            let raw = pixels
                .iter()
                .flat_map(|p| [p.r, p.g, p.b])
                .collect::<Vec<_>>();
            image::RgbImage::from_raw(760, 720, raw)
                .unwrap()
                .save(std::path::Path::new(&dir).join("settings-native.png"))
                .unwrap();
        }
        settings.hide().unwrap();
    }
}
