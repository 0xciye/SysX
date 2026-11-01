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
    export struct ConfigField { key: string, label: string, value: string, kind: string, choices: [ConfigOption], unit: string, key2: string, value2: string, hint: string, x: float, w: float, y: float, h: float, min: float, max: float, step: float }
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
        // Toggle mode: "ON"/"OFF"; empty hides the chip.
        in property <string> motion-toggle;
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
                if root.motion-toggle != "": chip := Rectangle {
                    property <color> ink: root.motion-toggle == "ON" ? #35d07f : #9aa9b4;
                    width: 86px; border-radius: 5px; border-width: 1px; border-color: self.ink.with-alpha(0.42); background: self.ink.with-alpha(0.10);
                    accessible-role: text;
                    accessible-label: "Motion toggle " + root.motion-toggle;
                    HorizontalLayout { padding-left: 9px; padding-right: 8px; spacing: 6px;
                        Rectangle { width: 7px; height: 7px; y: 10px; border-radius: 4px; background: chip.ink; }
                        Text { text: "MOTION " + root.motion-toggle; color: #ebf1f5; font-size: 11px; font-weight: 600; vertical-alignment: center; }
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

    export struct ConfigSection { label: string }
    global Palette {
        out property <color> window: #0a0d0f;
        out property <color> surface: #101315;
        out property <color> rail: #0d1012;
        out property <color> panel: #111618;
        out property <color> control: #151a1c;
        out property <color> control-hover: #1b2124;
        out property <color> text: #e6ebec;
        out property <color> text-secondary: #99a5a8;
        out property <color> muted: #788589;
        out property <color> border: #2b3336;
        out property <color> divider: #1c2326;
        out property <color> accent: #43cfbe;
        out property <color> on-accent: #061210;
        out property <color> good: #45c6a8;
        out property <color> waiting: #d9b763;
        out property <color> error: #e87979;
    }
    component SettingsButton inherits FocusScope {
        in property <string> caption;
        in property <string> kind: "secondary"; // primary | secondary | danger
        in property <bool> quiet;
        in property <string> font;
        callback activated();
        height: 30px;
        accessible-role: button;
        accessible-label: root.caption;
        key-pressed(event) => {
            if !event.repeat && (event.text == Key.Return || event.text == Key.Space) {
                root.activated();
                accept
            } else {
                reject
            }
        }
        Rectangle {
            border-radius: 6px;
            border-width: root.has-focus ? 2px : 1px;
            border-color: root.has-focus ? Palette.text
                : root.kind == "primary" ? Palette.accent
                : root.kind == "danger" ? Palette.error.with-alpha(0.55) : Palette.border;
            background: root.kind == "primary"
                ? (touch.pressed ? Palette.accent.darker(0.15) : (root.quiet ? Palette.accent.with-alpha(0.55) : Palette.accent))
                : (touch.pressed ? Palette.control-hover.darker(0.2) : (touch.has-hover ? Palette.control-hover : Palette.control));
            Text {
                text: root.caption;
                font-family: root.font;
                font-size: 13px;
                font-weight: root.kind == "primary" ? 600 : 500;
                color: root.kind == "primary" ? Palette.on-accent : (root.kind == "danger" ? Palette.error : Palette.text);
                horizontal-alignment: center; vertical-alignment: center;
                width: parent.width - 12px; overflow: elide;
            }
            touch := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.focus(); root.activated(); }
            }
        }
    }
    component SettingsSwitch inherits FocusScope {
        in property <bool> checked;
        in property <string> label;
        in property <string> font;
        callback toggled();
        width: 72px; height: 30px;
        accessible-role: checkbox;
        accessible-checked: root.checked;
        accessible-label: root.label;
        key-pressed(event) => {
            if !event.repeat && (event.text == Key.Return || event.text == Key.Space) {
                root.toggled();
                accept
            } else {
                reject
            }
        }
        Text {
            x: 0px; width: 26px; height: parent.height;
            text: root.checked ? "On" : "Off";
            font-family: root.font; font-size: 12px;
            color: root.checked ? Palette.text : Palette.muted;
            horizontal-alignment: right; vertical-alignment: center;
        }
        track := Rectangle {
            x: parent.width - 38px; y: (parent.height - 22px) / 2;
            width: 38px; height: 22px; border-radius: 11px;
            background: root.checked ? Palette.accent : (touch.has-hover ? #384246 : Palette.border);
            border-width: root.has-focus ? 2px : 0px; border-color: Palette.text;
            animate background { duration: 150ms; easing: ease-out; }
            Rectangle {
                width: 16px; height: 16px; border-radius: 8px; y: 3px;
                x: root.checked ? parent.width - 19px : 3px;
                background: root.checked ? Palette.on-accent : Palette.text;
                animate x { duration: 150ms; easing: ease-out; }
            }
        }
        touch := TouchArea {
            mouse-cursor: pointer;
            clicked => { root.focus(); root.toggled(); }
        }
    }
    component SettingsDropdown inherits FocusScope {
        in property <[ConfigOption]> options;
        in property <string> value;
        // Label of the selected option (supplied with the field).
        in property <string> display;
        in property <string> label;
        in property <string> font;
        callback picked(string);
        callback stepped();
        height: 36px;
        accessible-role: combobox;
        accessible-label: root.label;
        accessible-value: root.display;
        key-pressed(event) => {
            if event.repeat {
                return reject;
            }
            if event.text == Key.Return || event.text == Key.Space {
                popup.show();
                accept
            } else if event.text == Key.DownArrow {
                root.stepped();
                accept
            } else {
                reject
            }
        }
        Rectangle {
            border-radius: 6px;
            background: touch.has-hover ? Palette.control-hover : Palette.control;
            border-width: root.has-focus ? 2px : 1px;
            border-color: root.has-focus ? Palette.accent : Palette.border;
            Text {
                x: 12px; width: parent.width - 40px; height: parent.height;
                text: root.display; font-family: root.font; font-size: 13px;
                color: Palette.text; vertical-alignment: center; overflow: elide;
            }
            // A small down triangle built from rows (no font glyph needed).
            Rectangle {
                x: parent.width - 24px; y: (parent.height - 5px) / 2; width: 9px; height: 5px;
                for row in [9, 7, 5, 3, 1]: Rectangle {
                    x: (9 - row) * 0.5px; y: (9 - row) * 0.5px; width: row * 1px; height: 1px;
                    background: Palette.muted;
                }
            }
            touch := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.focus(); popup.show(); }
            }
        }
        popup := PopupWindow {
            x: 0px; y: root.height + 4px; width: root.width;
            height: root.options.length * 32px + 10px;
            Rectangle {
                border-radius: 8px; background: Palette.panel;
                border-width: 1px; border-color: Palette.border;
                drop-shadow-blur: 12px; drop-shadow-color: #00000080;
                VerticalLayout {
                    padding: 5px;
                    for option in root.options: Rectangle {
                        height: 32px; border-radius: 5px;
                        background: option-touch.has-hover ? Palette.control-hover
                            : (option.value == root.value ? Palette.control : transparent);
                        Text {
                            x: 10px; width: parent.width - 40px; height: parent.height;
                            text: option.label; font-family: root.font; font-size: 13px;
                            color: Palette.text; vertical-alignment: center; overflow: elide;
                        }
                        if option.value == root.value: Rectangle {
                            x: parent.width - 20px; y: (parent.height - 6px) / 2;
                            width: 6px; height: 6px; border-radius: 3px; background: Palette.accent;
                        }
                        option-touch := TouchArea {
                            mouse-cursor: pointer;
                            clicked => { root.picked(option.value); }
                        }
                    }
                }
            }
        }
    }
    component SettingsSlider inherits FocusScope {
        in property <string> label;
        in property <float> value;
        in property <float> minimum;
        in property <float> maximum: 1;
        in property <float> step: 0.01;
        in property <string> unit;
        in property <string> font;
        in property <string> mono-font;
        callback committed(string);
        // The shown value; follows `value` until dragged (rows are rebuilt
        // from the settings draft after each change).
        in-out property <float> live: root.value;
        property <float> span: max(0.000001, root.maximum - root.minimum);
        property <float> fraction: clamp((root.live - root.minimum) / root.span, 0, 1);
        property <string> shown: "" + (round(root.live * 100) / 100) + root.unit;
        function snapped(v: float) -> float {
            return clamp(round((v - root.minimum) / root.step) * root.step + root.minimum,
                root.minimum, root.maximum);
        }
        function commit() {
            root.committed("" + (round(root.live * 1000) / 1000));
        }
        height: 44px;
        accessible-role: slider;
        accessible-label: root.label;
        accessible-value: root.shown;
        accessible-value-minimum: root.minimum;
        accessible-value-maximum: root.maximum;
        accessible-value-step: root.step;
        key-pressed(event) => {
            if event.text == Key.LeftArrow || event.text == Key.DownArrow {
                root.live = root.snapped(root.live - root.step);
                root.commit();
                accept
            } else if event.text == Key.RightArrow || event.text == Key.UpArrow {
                root.live = root.snapped(root.live + root.step);
                root.commit();
                accept
            } else {
                reject
            }
        }
        Text {
            x: 0px; y: 0px; width: parent.width - 70px; height: 18px;
            text: root.label; font-family: root.font; font-size: 13px; font-weight: 600;
            color: Palette.text; vertical-alignment: center; overflow: elide;
        }
        Text {
            x: parent.width - 70px; y: 0px; width: 70px; height: 18px;
            text: root.shown; font-family: root.mono-font; font-size: 13px;
            color: Palette.text; horizontal-alignment: right; vertical-alignment: center;
        }
        Rectangle {
            x: 7px; y: 30px; width: parent.width - 14px; height: 4px; border-radius: 2px;
            background: Palette.border;
            Rectangle {
                x: 0px; width: parent.width * root.fraction; height: parent.height;
                border-radius: 2px; background: Palette.text;
            }
        }
        Rectangle {
            x: (parent.width - 14px) * root.fraction; y: 25px;
            width: 14px; height: 14px; border-radius: 7px;
            background: Palette.text;
            border-width: root.has-focus ? 3px : 1px;
            border-color: root.has-focus ? Palette.accent : Palette.window;
        }
        drag := TouchArea {
            y: 18px; height: parent.height - 18px;
            mouse-cursor: pointer;
            function at(x: length) {
                root.live = root.snapped(root.minimum
                    + clamp((x - 7px) / max(1px, self.width - 14px), 0, 1) * root.span);
            }
            pointer-event(event) => {
                if event.kind == PointerEventKind.down {
                    root.focus();
                    self.at(self.mouse-x);
                } else if event.kind == PointerEventKind.up {
                    root.commit();
                }
            }
            moved => {
                if self.pressed {
                    self.at(self.mouse-x);
                }
            }
        }
    }
    component SettingsEngineCard inherits FocusScope {
        in property <string> caption;
        in property <string> detail;
        in property <bool> selected;
        in property <string> font;
        callback picked();
        accessible-role: tab;
        accessible-checked: root.selected;
        accessible-label: root.caption;
        key-pressed(event) => {
            if !event.repeat && (event.text == Key.Return || event.text == Key.Space) {
                root.picked();
                accept
            } else {
                reject
            }
        }
        Rectangle {
            border-radius: 8px;
            background: touch.has-hover ? Palette.control-hover : Palette.control;
            border-width: root.has-focus ? 2px : 1px;
            border-color: root.has-focus ? Palette.text
                : (root.selected ? Palette.accent.with-alpha(0.7) : Palette.border);
            Rectangle {
                x: 16px; y: (parent.height - 16px) / 2; width: 16px; height: 16px;
                border-radius: 8px; border-width: 1px;
                border-color: root.selected ? Palette.text : Palette.muted;
                background: transparent;
                if root.selected: Rectangle {
                    x: 4px; y: 4px; width: 8px; height: 8px; border-radius: 4px;
                    background: Palette.text;
                }
            }
            Text {
                x: 44px; y: (parent.height / 2) - 19px; width: parent.width - 56px; height: 20px;
                text: root.caption; font-family: root.font; font-size: 13px; font-weight: 600;
                color: Palette.text; vertical-alignment: center; overflow: elide;
            }
            Text {
                x: 44px; y: parent.height / 2 + 1px; width: parent.width - 56px; height: 18px;
                text: root.detail; font-family: root.font; font-size: 12px;
                color: Palette.text-secondary; vertical-alignment: center; overflow: elide;
            }
            touch := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.focus(); root.picked(); }
            }
        }
    }
    component SettingsInput inherits Rectangle {
        in property <string> text;
        in property <string> unit;
        in property <bool> invalid;
        in property <bool> mono: true;
        in property <string> font;
        in property <string> mono-font;
        in property <string> label;
        callback edited(string);
        height: 36px; border-radius: 6px;
        background: Palette.control;
        border-width: input.has-focus || root.invalid ? 2px : 1px;
        border-color: root.invalid ? Palette.error : (input.has-focus ? Palette.accent : Palette.border);
        input := TextInput {
            x: 12px;
            width: parent.width - self.x - (root.unit != "" ? 40px : 12px);
            height: parent.height;
            text: root.text;
            font-family: root.mono ? root.mono-font : root.font;
            font-size: 13px;
            color: Palette.text;
            selection-background-color: Palette.accent.with-alpha(0.35);
            selection-foreground-color: Palette.text;
            vertical-alignment: center;
            single-line: true;
            accessible-label: root.label;
            edited => { root.edited(self.text); }
        }
        if root.unit != "": Text {
            x: parent.width - 38px; width: 28px; height: parent.height;
            text: root.unit; font-family: root.font; font-size: 12px;
            color: Palette.muted; horizontal-alignment: right; vertical-alignment: center;
        }
    }
    component SettingsNavItem inherits FocusScope {
        in property <string> caption;
        in property <bool> active;
        in property <string> font;
        callback activated();
        height: 32px;
        accessible-role: tab;
        accessible-label: root.caption;
        key-pressed(event) => {
            if !event.repeat && (event.text == Key.Return || event.text == Key.Space) {
                root.activated();
                accept
            } else {
                reject
            }
        }
        Rectangle {
            border-radius: 6px;
            background: root.active ? Palette.accent.with-alpha(0.12) : (touch.has-hover ? Palette.control : transparent);
            border-width: root.has-focus ? 2px : 0px; border-color: Palette.text;
            Rectangle {
                x: 0px; y: 8px; width: 2px; height: parent.height - 16px; border-radius: 1px;
                background: root.active ? Palette.accent : transparent;
            }
            Text {
                x: 12px; width: parent.width - 20px; height: parent.height;
                text: root.caption; font-family: root.font; font-size: 13px;
                font-weight: root.active ? 600 : 400;
                color: root.active ? Palette.text : Palette.text-secondary;
                vertical-alignment: center; overflow: elide;
            }
            touch := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.focus(); root.activated(); }
            }
        }
    }

    export component SettingsWindow inherits Window {
        title: "SysX Settings";
        preferred-width: 940px; preferred-height: 780px;
        min-width: 760px; min-height: 600px;
        background: Palette.window;
        default-font-family: root.body-font;
        in property <string> body-font: "Segoe UI";
        in property <string> mono-font: "Consolas";
        in property <[ConfigField]> fields;
        in property <[ConfigSection]> sections;
        in property <float> content-height;
        in property <string> profile-name;
        in property <string> message;
        // "", "success", "error" or "info"
        in property <string> message-kind;
        in property <bool> dirty;
        in property <string> invalid-key;
        callback field-edited(string, string);
        callback field-activated(string);
        callback field-selected(string, string);
        callback apply-requested();
        callback close-requested();
        // The page shown: one per section in the rail.
        in-out property <int> current-page;
        callback page-selected(int);
        callback reset-scroll();
        reset-scroll => { settings-list.viewport-y = 0px; }
        property <color> status-color: root.message-kind == "success" ? Palette.good
            : root.message-kind == "error" ? Palette.error
            : root.message-kind == "info" ? Palette.text-secondary
            : root.dirty ? Palette.waiting : Palette.muted;
        VerticalLayout {
            // Header: what this window is and which profile is being edited.
            Rectangle {
                height: 60px; background: Palette.surface;
                HorizontalLayout {
                    padding-left: 20px; padding-right: 16px; spacing: 12px;
                    Text {
                        text: "Settings"; color: Palette.text;
                        font-size: 18px; font-weight: 600; vertical-alignment: center;
                    }
                    Rectangle {
                        y: (parent.height - 24px) / 2; height: 24px;
                        width: profile-label.preferred-width + 20px;
                        border-radius: 12px; background: Palette.control;
                        border-width: 1px; border-color: Palette.border;
                        profile-label := Text {
                            text: "Profile · " + root.profile-name;
                            color: Palette.text-secondary; font-size: 12px;
                            horizontal-alignment: center; vertical-alignment: center;
                            width: parent.width;
                        }
                    }
                    Rectangle { horizontal-stretch: 1; }
                    SettingsButton {
                        y: (parent.height - 30px) / 2; width: 72px;
                        caption: "Close"; font: root.body-font;
                        activated => { root.close-requested(); }
                    }
                }
                Rectangle { y: parent.height - 1px; height: 1px; background: Palette.border; }
            }
            HorizontalLayout {
                vertical-stretch: 1;
                // Page rail: each section is its own page.
                Rectangle {
                    width: 168px; background: Palette.rail;
                    VerticalLayout {
                        padding: 12px; spacing: 2px; alignment: start;
                        for section[index] in root.sections: SettingsNavItem {
                            caption: section.label; font: root.body-font;
                            active: index == root.current-page;
                            activated => {
                                if index != root.current-page {
                                    root.current-page = index;
                                    settings-list.viewport-y = 0px;
                                    root.page-selected(index);
                                }
                            }
                        }
                    }
                    Rectangle { x: parent.width - 1px; width: 1px; background: Palette.border; }
                }
                settings-list := ScrollView {
                    horizontal-stretch: 1;
                    viewport-height: root.content-height * 1px;
                    viewport-width: self.visible-width;
                    content := Rectangle {
                        width: settings-list.visible-width;
                        height: root.content-height * 1px;
                        // Page margin, card padding and the gap between cells.
                        property <length> pad: 20px;
                        property <length> card-pad: 18px;
                        property <length> gap: 16px;
                        property <length> inner-x: self.pad + self.card-pad;
                        property <length> inner-w: self.width - 2 * (self.pad + self.card-pad);
                        for field in root.fields: cell := Rectangle {
                            x: field.kind == "section" ? content.pad
                                : content.inner-x + field.x * (content.inner-w + content.gap);
                            y: field.y * 1px;
                            width: field.kind == "section" ? content.width - 2 * content.pad
                                : field.w * (content.inner-w + content.gap) - content.gap;
                            height: field.h * 1px;
                            property <bool> is-invalid: root.invalid-key != ""
                                && (root.invalid-key == field.key || root.invalid-key == field.key2);
                            property <bool> labeled: field.kind == "dropdown" || field.kind == "number"
                                || field.kind == "text" || field.kind == "info"
                                || field.kind == "rename" || field.kind == "profile-actions";
                            if field.kind == "section": Rectangle {
                                Text {
                                    x: 2px; y: 0px; height: 22px;
                                    text: field.label; color: Palette.text;
                                    font-size: 15px; font-weight: 600; vertical-alignment: center;
                                }
                                Rectangle {
                                    y: 32px; height: parent.height - 32px;
                                    border-radius: 10px; background: Palette.panel;
                                    border-width: 1px; border-color: Palette.border;
                                }
                            }
                            if field.kind == "subhead": Text {
                                y: 0px; height: 22px; width: parent.width;
                                text: field.label; color: Palette.text;
                                font-size: 14px; font-weight: 600; vertical-alignment: center;
                            }
                            if cell.labeled: Text {
                                y: 0px; height: 18px; width: parent.width;
                                text: field.label; color: Palette.text;
                                font-size: 13px; font-weight: 600; vertical-alignment: center;
                                overflow: elide;
                            }
                            if field.hint != "": Text {
                                y: parent.height - 16px; height: 16px; width: parent.width;
                                text: field.hint; color: Palette.muted;
                                font-size: 12px; vertical-alignment: center; overflow: elide;
                            }
                            if field.kind == "toggle": Rectangle {
                                y: 0px; height: 36px;
                                Text {
                                    x: 0px; width: parent.width - 80px; height: parent.height;
                                    text: field.label; color: Palette.text; font-size: 13px; font-weight: 600;
                                    vertical-alignment: center; overflow: elide;
                                }
                                SettingsSwitch {
                                    x: parent.width - self.width; y: (parent.height - self.height) / 2;
                                    checked: field.value == "true"; label: field.label; font: root.body-font;
                                    toggled => { root.field-activated(field.key); }
                                }
                            }
                            if field.kind == "dropdown": SettingsDropdown {
                                y: 24px; width: parent.width;
                                options: field.choices; value: field.value; display: field.value2;
                                label: field.label; font: root.body-font;
                                picked(value) => { root.field-selected(field.key, value); }
                                stepped => { root.field-activated(field.key); }
                            }
                            if field.kind == "number" || field.kind == "text": SettingsInput {
                                y: 24px; width: parent.width;
                                text: field.value; unit: field.unit; label: field.label;
                                mono: field.kind == "number";
                                invalid: cell.is-invalid;
                                font: root.body-font; mono-font: root.mono-font;
                                edited(text) => { root.field-edited(field.key, text); }
                            }
                            if field.kind == "slider": SettingsSlider {
                                y: 0px; width: parent.width;
                                label: field.label; value: field.value.to-float(); unit: field.unit;
                                minimum: field.min; maximum: field.max; step: field.step;
                                font: root.body-font; mono-font: root.mono-font;
                                committed(text) => { root.field-edited(field.key, text); }
                            }
                            if field.kind == "info": Rectangle {
                                y: 24px; height: 36px;
                                border-radius: 6px; background: Palette.control;
                                border-width: 1px; border-color: Palette.border;
                                Text {
                                    x: 12px; width: parent.width - 24px; height: parent.height;
                                    text: field.value; color: Palette.text-secondary; font-size: 13px;
                                    font-family: root.mono-font; vertical-alignment: center; overflow: elide;
                                }
                            }
                            if field.kind == "engine": HorizontalLayout {
                                spacing: content.gap;
                                SettingsEngineCard {
                                    caption: "Pixel Seek"; detail: "Fast pixel-based detection";
                                    selected: field.value == "pixel_seek"; font: root.body-font;
                                    picked => { root.field-selected("strategy", "pixel_seek"); }
                                }
                                SettingsEngineCard {
                                    caption: "Object Detection"; detail: "AI model recognition";
                                    selected: field.value == "object_detection"; font: root.body-font;
                                    picked => { root.field-selected("strategy", "object_detection"); }
                                }
                            }
                            if field.kind == "offset": Rectangle {
                                // The center cross is the target; the dot is where
                                // the offsets aim relative to it.
                                height: 56px; border-radius: 8px; background: Palette.control;
                                border-width: 1px; border-color: Palette.border;
                                property <length> dx: clamp(field.value.to-float() * 2px,
                                    -(self.width / 2 - 10px), self.width / 2 - 10px);
                                property <length> dy: clamp(field.value2.to-float() * 2px,
                                    -(self.height / 2 - 6px), self.height / 2 - 6px);
                                Rectangle {
                                    x: parent.width / 2 - 8px; y: parent.height / 2; width: 16px; height: 1px;
                                    background: Palette.text-secondary;
                                }
                                Rectangle {
                                    x: parent.width / 2; y: parent.height / 2 - 8px; width: 1px; height: 16px;
                                    background: Palette.text-secondary;
                                }
                                Rectangle {
                                    x: parent.width / 2 + parent.dx - 4px; y: parent.height / 2 + parent.dy - 4px;
                                    width: 8px; height: 8px; border-radius: 4px; background: Palette.accent;
                                }
                                Rectangle {
                                    x: parent.width - self.width - 8px; y: parent.height - 24px;
                                    width: readout.preferred-width + 12px; height: 18px; border-radius: 4px;
                                    background: Palette.panel;
                                    readout := Text {
                                        width: parent.width; height: parent.height;
                                        text: field.unit; font-family: root.mono-font; font-size: 11px;
                                        color: Palette.text-secondary;
                                        horizontal-alignment: center; vertical-alignment: center;
                                    }
                                }
                            }
                            if field.kind == "rename": HorizontalLayout {
                                y: 24px; height: 36px; spacing: 8px;
                                SettingsInput {
                                    text: field.value; mono: false; label: field.label;
                                    font: root.body-font; mono-font: root.mono-font;
                                    edited(text) => { root.field-edited(field.key, text); }
                                }
                                SettingsButton {
                                    y: 3px; width: 84px; caption: "Rename"; font: root.body-font;
                                    activated => { root.field-activated(field.key2); }
                                }
                            }
                            if field.kind == "profile-actions": HorizontalLayout {
                                y: 24px; height: 36px; spacing: 8px;
                                for action in field.choices: SettingsButton {
                                    y: 3px; horizontal-stretch: 1; caption: action.label; font: root.body-font;
                                    kind: action.value == "profile.delete" ? "danger" : "secondary";
                                    activated => { root.field-activated(action.value); }
                                }
                            }
                        }
                    }
                }
            }
            // Status and the one global Apply action.
            Rectangle {
                height: 56px; background: Palette.surface;
                Rectangle { y: 0px; height: 1px; background: Palette.border; }
                HorizontalLayout {
                    padding-left: 20px; padding-right: 16px; spacing: 10px;
                    Rectangle {
                        width: 8px; height: 8px; y: (parent.height - 8px) / 2; border-radius: 4px;
                        background: root.status-color;
                    }
                    Text {
                        horizontal-stretch: 1;
                        text: root.message != "" ? root.message : (root.dirty ? "Unsaved changes" : "All changes applied");
                        color: root.message != "" || root.dirty ? root.status-color : Palette.text-secondary;
                        font-size: 13px; vertical-alignment: center; overflow: elide;
                    }
                    SettingsButton {
                        y: (parent.height - 34px) / 2; width: 136px; height: 34px;
                        caption: "Apply Changes"; kind: "primary"; quiet: !root.dirty; font: root.body-font;
                        activated => { root.apply-requested(); }
                    }
                }
            }
        }
    }
}
fn ink(rgb: u32) -> slint::Color {
    slint::Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Settings page geometry (logical px): each section is its own page with a
/// heading over one card; inside the card, rows of cells laid out on a grid
/// (label above the control, an optional hint below).
const SETTINGS_TOP: f32 = 20.0;
/// Section title above its card.
const SETTINGS_HEADING: f32 = 32.0;
const CARD_PAD: f32 = 18.0;
const ROW_GAP: f32 = 16.0;
const HINT_HEIGHT: f32 = 20.0;

/// One page of native Settings: a rail entry and its laid-out cells.
struct SettingsPageLayout {
    section: ConfigSection,
    fields: Vec<ConfigField>,
    height: f32,
}

/// Smoothing and offsets are whole numbers (user request); the speed
/// booster keeps its decimals.
fn whole_number_key(key: &str) -> bool {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    leaf.starts_with("smoothing") || leaf.starts_with("smooth") || leaf.starts_with("offset")
}

fn setting_text(settings: &serde_json::Value, key: &str) -> String {
    if whole_number_key(key) {
        if let Some(number) = key
            .split('.')
            .try_fold(settings, |value, part| value.get(part))
            .and_then(serde_json::Value::as_f64)
        {
            return format!("{}", number.round() as i64);
        }
    }
    key.split('.')
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
        .unwrap_or_default()
}

fn options(items: &[(&str, &str)]) -> ModelRc<ConfigOption> {
    ModelRc::new(VecModel::from(
        items
            .iter()
            .map(|(label, value)| ConfigOption {
                label: (*label).into(),
                value: (*value).into(),
            })
            .collect::<Vec<_>>(),
    ))
}

const BUTTONS: &[(&str, &str)] = &[
    ("Right click", "right"),
    ("Mouse button 4", "m4"),
    ("Mouse button 5", "m5"),
];
const DYNAMIC_FOV_KEYS: &[(&str, &str)] = &[
    ("Off", "off"),
    ("Right click", "right"),
    ("Mouse button 4", "m4"),
    ("Mouse button 5", "m5"),
];
const MAGNET_MODES: &[(&str, &str)] = &[
    ("Spray (hold fire)", "spray"),
    ("Tap fire", "tap"),
    ("Flick", "silent_immediate"),
    ("Auto (center crossing)", "auto"),
];

/// Fixed dropdown choices per setting key, as (label, value).
fn choice_options(key: &str) -> &'static [(&'static str, &'static str)] {
    match key {
        "pixelColor" => &[("Yellow", "yellow"), ("Red", "red"), ("Purple", "purple")],
        "scale" => &[("Smooth", "Smooth"), ("Pixel perfect", "Pixel perfect")],
        "triggerButton" | "magnet.triggerButton" => BUTTONS,
        "dynamicFovKey" | "magnet.dynamicFovKey" => DYNAMIC_FOV_KEYS,
        "modelBackend" => MODEL_BACKENDS,
        "computeTarget" => &[("CPU", "cpu"), ("GPU", "gpu"), ("NPU", "npu")],
        "modelPrecision" => &[("FP16", "fp16"), ("INT8", "int8")],
        "colorSwap" => &[
            ("Off", "off"),
            ("Yellow", "yellow"),
            ("Red", "red"),
            ("Purple", "purple"),
        ],
        "magnet.mode" => MAGNET_MODES,
        "recoil.recoilPattern" => &[
            ("Vandal", "vandal"),
            ("Phantom", "phantom"),
            ("Custom", "custom"),
        ],
        _ => &[],
    }
}

fn field(key: &str, label: &str, kind: &str) -> ConfigField {
    ConfigField {
        key: key.into(),
        label: label.into(),
        value: Default::default(),
        kind: kind.into(),
        choices: ModelRc::default(),
        unit: Default::default(),
        key2: Default::default(),
        value2: Default::default(),
        hint: Default::default(),
        x: 0.0,
        w: 1.0,
        y: 0.0,
        h: 0.0,
        min: 0.0,
        max: 1.0,
        step: 0.01,
    }
}

/// Height of one cell by its kind, plus the hint line.
fn cell_height(cell: &ConfigField) -> f32 {
    let body = match cell.kind.as_str() {
        "subhead" => 22.0,
        "toggle" => 36.0,
        "slider" => 44.0,
        "engine" => 66.0,
        "offset" => 56.0,
        _ => 60.0,
    };
    body + if cell.hint.is_empty() {
        0.0
    } else {
        HINT_HEIGHT
    }
}

fn with_hint(mut cell: ConfigField, hint: &str) -> ConfigField {
    cell.hint = hint.into();
    cell
}

/// A dropdown over `choices`; `value2` carries the selected label.
fn dropdown_of(
    settings: &serde_json::Value,
    key: &str,
    label: &str,
    choices: Vec<(String, String)>,
) -> ConfigField {
    let mut cell = field(key, label, "dropdown");
    let value = setting_text(settings, key);
    cell.value2 = choices
        .iter()
        .find(|(_, choice)| *choice == value)
        .map(|(label, _)| label.clone())
        .unwrap_or_else(|| value.clone())
        .into();
    cell.value = value.into();
    cell.choices = ModelRc::new(VecModel::from(
        choices
            .into_iter()
            .map(|(label, value)| ConfigOption {
                label: label.into(),
                value: value.into(),
            })
            .collect::<Vec<_>>(),
    ));
    cell
}

fn dropdown(settings: &serde_json::Value, key: &str, label: &str) -> ConfigField {
    let choices = choice_options(key)
        .iter()
        .map(|(label, value)| ((*label).to_owned(), (*value).to_owned()))
        .collect();
    dropdown_of(settings, key, label, choices)
}

fn number(settings: &serde_json::Value, key: &str, label: &str, unit: &str) -> ConfigField {
    let mut cell = field(key, label, "number");
    cell.value = setting_text(settings, key).into();
    cell.unit = unit.into();
    cell
}

fn toggle(settings: &serde_json::Value, key: &str, label: &str) -> ConfigField {
    let mut cell = field(key, label, "toggle");
    cell.value = setting_text(settings, key).into();
    cell
}

fn slider(
    settings: &serde_json::Value,
    key: &str,
    label: &str,
    (min, max, step): (f32, f32, f32),
    unit: &str,
) -> ConfigField {
    let mut cell = field(key, label, "slider");
    cell.value = setting_text(settings, key).into();
    (cell.min, cell.max, cell.step) = (min, max, step);
    cell.unit = unit.into();
    cell
}

fn subhead(label: &str) -> ConfigField {
    field("", label, "subhead")
}

/// The offset pad: the center cross is the target, the dot the offset aim.
fn offset_pad(settings: &serde_json::Value, key_x: &str, key_y: &str) -> ConfigField {
    let mut cell = field(key_x, "Offset", "offset");
    cell.value = setting_text(settings, key_x).into();
    cell.key2 = key_y.into();
    cell.value2 = setting_text(settings, key_y).into();
    let signed = |key: &str| {
        let value = setting_text(settings, key).parse::<f64>().unwrap_or(0.0);
        format!("{}{}", if value < 0.0 { "" } else { "+" }, value)
    };
    cell.unit = format!("{}, {}", signed(key_x), signed(key_y)).into();
    cell
}

fn is_on(settings: &serde_json::Value, key: &str) -> bool {
    key.split('.')
        .try_fold(settings, |value, part| value.get(part))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// Collects the Settings pages: a section per page, rows of cells inside.
#[derive(Default)]
struct SettingsPage {
    pages: Vec<SettingsPageLayout>,
}

impl SettingsPage {
    fn section(&mut self, label: &str) {
        let mut heading = field("", label, "section");
        heading.y = SETTINGS_TOP;
        self.pages.push(SettingsPageLayout {
            section: ConfigSection {
                label: label.into(),
            },
            fields: vec![heading],
            height: SETTINGS_TOP + SETTINGS_HEADING + CARD_PAD,
        });
    }

    /// One row of equally wide cells.
    fn row(&mut self, cells: Vec<ConfigField>) {
        let Some(page) = self.pages.last_mut() else {
            return;
        };
        let columns = cells.len().max(1) as f32;
        let height = cells.iter().map(cell_height).fold(0.0, f32::max);
        for (index, mut cell) in cells.into_iter().enumerate() {
            cell.x = index as f32 / columns;
            cell.w = 1.0 / columns;
            cell.y = page.height;
            cell.h = cell_height(&cell);
            page.fields.push(cell);
        }
        page.height += height + ROW_GAP;
    }

    /// Closes each card under its section title.
    fn layout(mut self) -> Vec<SettingsPageLayout> {
        for page in &mut self.pages {
            let bottom = page.height - ROW_GAP + CARD_PAD;
            page.fields[0].h = bottom - SETTINGS_TOP;
            page.height = bottom + SETTINGS_TOP;
        }
        self.pages
    }
}

/// `models`: the built-in models as (id, label).
fn settings_page(
    settings: &serde_json::Value,
    models: &[(String, String)],
) -> Vec<SettingsPageLayout> {
    let mut page = SettingsPage::default();

    page.section("Profiles");
    let names: Vec<(String, String)> = settings
        .get("profiles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|profile| profile.get("name").and_then(serde_json::Value::as_str))
        .map(|name| (name.to_owned(), name.to_owned()))
        .collect();
    let active_name = profile_name_of(settings);
    let mut select = dropdown_of(settings, "profile.select", "Active profile", names);
    select.value = active_name.clone().into();
    select.value2 = active_name.clone().into();
    let mut rename = field("profile.rename", "Name", "rename");
    rename.value = active_name.into();
    rename.key2 = "profile.rename.apply".into();
    page.row(vec![
        with_hint(select, "The middle mouse button cycles profiles"),
        rename,
    ]);
    let mut actions = field("profile.actions", "Manage", "profile-actions");
    actions.choices = options(&[
        ("New", "profile.create"),
        ("Duplicate", "profile.clone"),
        ("Delete", "profile.delete"),
    ]);
    page.row(vec![actions]);
    // Quick switches for the active profile (user request); the same
    // settings as the toggles on the Motion, Magnet and Recoil pages.
    page.row(vec![subhead("Modules")]);
    page.row(vec![
        toggle(settings, "motionEnabled", "Motion"),
        toggle(settings, "magnet.enabled", "Magnet"),
        toggle(settings, "recoil.recoilEnabled", "Recoil"),
    ]);

    page.section("Detection");
    let mut engine = field("strategy", "Engine", "engine");
    engine.value = setting_text(settings, "strategy").into();
    page.row(vec![engine]);
    let sensitivity = with_hint(
        number(settings, "sens", "In-game sensitivity", ""),
        "Your in-game mouse sensitivity",
    );
    if engine_is_object_detection(settings) {
        page.row(vec![subhead("AI Model Settings")]);
        let model = if models.is_empty() {
            let mut none = field("activeModelId", "Model", "info");
            none.value = "None built in".into();
            none
        } else {
            dropdown_of(
                settings,
                "activeModelId",
                "Model",
                models
                    .iter()
                    .map(|(id, label)| (label.clone(), id.clone()))
                    .collect(),
            )
        };
        page.row(vec![
            with_hint(model, "Built-in AI model"),
            with_hint(
                dropdown(settings, "modelBackend", "Runtime"),
                "Inference backend",
            ),
        ]);
        // Only OpenVINO picks its device; the GPU runtimes imply theirs.
        let openvino = setting_text(settings, "modelBackend") == "openvino";
        let precision = with_hint(
            dropdown(settings, "modelPrecision", "Compute"),
            "Precision mode",
        );
        let color_swap = with_hint(
            dropdown(settings, "colorSwap", "Color swap"),
            "Only detect enemies with this outline color",
        );
        let target_color = with_hint(
            dropdown(settings, "pixelColor", "Target color"),
            "Color used by the color filter",
        );
        let confidence = slider(
            settings,
            "modelConfidence",
            "Min confidence",
            (0.0, 1.0, 0.01),
            "",
        );
        if openvino {
            page.row(vec![
                with_hint(
                    dropdown(settings, "computeTarget", "Device"),
                    "Processing unit",
                ),
                precision,
            ]);
            page.row(vec![color_swap, target_color]);
            page.row(vec![sensitivity]);
        } else {
            page.row(vec![precision, color_swap]);
            page.row(vec![target_color, sensitivity]);
        }
        page.row(vec![
            confidence,
            slider(
                settings,
                "abilityConfidence",
                "Min ability confidence",
                (0.0, 1.0, 0.01),
                "",
            ),
        ]);
    } else {
        page.row(vec![subhead("Pixel Seek")]);
        page.row(vec![
            with_hint(
                dropdown(settings, "pixelColor", "Target color"),
                "Target outline color",
            ),
            sensitivity,
        ]);
    }
    page.row(vec![subhead("Capture")]);
    // Object Detection captures the model's input size instead.
    if engine_is_object_detection(settings) {
        page.row(vec![dropdown(settings, "scale", "Preview scaling")]);
    } else {
        page.row(vec![
            number(settings, "cropResolution", "Crop resolution", "px"),
            dropdown(settings, "scale", "Preview scaling"),
        ]);
    }
    page.row(vec![with_hint(
        toggle(settings, "colorFilter", "Color filter"),
        "Black out everything but the target color before detection",
    )]);

    page.section("Motion");
    page.row(vec![
        with_hint(
            toggle(settings, "motionAlwaysOn", "Always on"),
            "Off with Toggle off: hold the trigger",
        ),
        with_hint(
            toggle(settings, "motionToggle", "Toggle"),
            "Each trigger press turns Motion on or off",
        ),
    ]);
    page.row(vec![
        number(settings, "fovDegrees", "FOV", "°"),
        dropdown(settings, "triggerButton", "Trigger button"),
    ]);
    page.row(vec![
        number(settings, "dynamicFovDegrees", "Dynamic FOV size", "°"),
        dropdown(settings, "dynamicFovKey", "Dynamic FOV key"),
    ]);
    page.row(vec![
        number(settings, "offsetX", "X offset", "px"),
        number(settings, "offsetY", "Y offset", "px"),
    ]);
    page.row(vec![offset_pad(settings, "offsetX", "offsetY")]);
    // Stable Normal motion (user request, 2026-11-01): Speed per axis, and a
    // smoothing strength that applies only inside the Smooth FOV.
    page.row(vec![
        with_hint(
            number(settings, "motionSpeedX", "Speed X", ""),
            "Fraction of the error per frame (0.01–1)",
        ),
        with_hint(
            number(settings, "motionSpeedY", "Speed Y", ""),
            "Fraction of the error per frame (0.01–1)",
        ),
    ]);
    page.row(vec![with_hint(
        number(settings, "smoothFovDegrees", "Smooth FOV", "°"),
        "Smoothing applies inside this circle, fading out at the target",
    )]);
    smoothing_rows(
        &mut page,
        settings,
        [
            "smoothingXMin",
            "smoothingXMax",
            "smoothingYMin",
            "smoothingYMax",
        ],
    );
    booster_rows(
        &mut page,
        settings,
        ["speedBooster", "speedNear", "speedMid", "speedFar"],
    );
    page.row(vec![with_hint(
        toggle(settings, "continueInsideBox", "No deadzone"),
        "Keep moving within 1 px of the target",
    )]);
    page.row(vec![toggle(
        settings,
        "motionRenderer.enabled",
        "ABCurves texture",
    )]);

    page.section("Magnet");
    let mode = setting_text(settings, "magnet.mode");
    page.row(vec![
        number(settings, "magnet.fovDegrees", "FOV", "°"),
        dropdown(settings, "magnet.triggerButton", "Trigger button"),
        dropdown(settings, "magnet.mode", "Magnet mode"),
    ]);
    // Each press snaps to the target, then: Tap one shot, Spray a burst,
    // Flick one shot and back; Auto keeps its center-crossing timing.
    match mode.as_str() {
        "spray" => page.row(vec![with_hint(
            number(settings, "magnet.forcedReleaseMs", "Max burst", "ms"),
            "How long one burst holds the button",
        )]),
        "auto" => page.row(vec![
            with_hint(
                number(settings, "magnet.delayMs", "Delay", "ms"),
                "Before firing",
            ),
            with_hint(
                number(settings, "magnet.cooldownMs", "Cooldown", "ms"),
                "Between shots",
            ),
            with_hint(
                number(settings, "magnet.sleepMs", "Sleep", "ms"),
                "Between checks",
            ),
        ]),
        _ => {}
    }
    page.row(vec![
        number(
            settings,
            "magnet.dynamicFovDegrees",
            "Dynamic FOV size",
            "°",
        ),
        dropdown(settings, "magnet.dynamicFovKey", "Dynamic FOV key"),
    ]);
    page.row(vec![
        number(settings, "magnet.offsetX", "X offset", "px"),
        number(settings, "magnet.offsetY", "Y offset", "px"),
    ]);
    page.row(vec![offset_pad(
        settings,
        "magnet.offsetX",
        "magnet.offsetY",
    )]);
    // Only a spray burst pulls after the snap.
    if mode == "spray" {
        smoothing_rows(
            &mut page,
            settings,
            [
                "magnet.smoothXMin",
                "magnet.smoothXMax",
                "magnet.smoothYMin",
                "magnet.smoothYMax",
            ],
        );
        booster_rows(
            &mut page,
            settings,
            [
                "magnet.speedBooster",
                "magnet.speedNear",
                "magnet.speedMid",
                "magnet.speedFar",
            ],
        );
    }

    page.section("Recoil");
    page.row(vec![with_hint(
        dropdown(settings, "recoil.recoilPattern", "Pattern"),
        "Pull per bullet; Custom uses your own list",
    )]);
    page.row(vec![with_hint(
        slider(
            settings,
            "recoil.recoilPercentage",
            "Recoil reduction factor",
            (0.0, 100.0, 1.0),
            "%",
        ),
        "Higher values = more recoil compensation",
    )]);
    page.row(vec![
        with_hint(
            number(settings, "recoil.recoilStartDelayMs", "Start delay", "ms"),
            "Delay before compensation starts",
        ),
        with_hint(
            number(
                settings,
                "recoil.recoilLevelDurationMs",
                "Time per bullet",
                "ms",
            ),
            "Time per pattern step",
        ),
    ]);
    if setting_text(settings, "recoil.recoilPattern") == "custom" {
        let mut levels = field("recoil.recoilLevels", "Recoil pattern", "text");
        levels.value = setting_text(settings, "recoil.recoilLevels").into();
        page.row(vec![with_hint(
            levels,
            "Comma-separated pull per bullet; the last repeats",
        )]);
    }

    page.section("Connection");
    page.row(vec![with_hint(
        number(settings, "port", "UDP input port", ""),
        "OBS sender port (default 4200)",
    )]);
    page.layout()
}

/// X and Y smoothing, each a Min and a Max slider under its heading.
fn smoothing_rows(page: &mut SettingsPage, settings: &serde_json::Value, keys: [&str; 4]) {
    let range = (1.0, 50.0, 1.0);
    page.row(vec![subhead("X Smooth"), subhead("Y Smooth")]);
    page.row(vec![
        slider(settings, keys[0], "Min", range, ""),
        slider(settings, keys[2], "Min", range, ""),
    ]);
    page.row(vec![
        slider(settings, keys[1], "Max", range, ""),
        slider(settings, keys[3], "Max", range, ""),
    ]);
}

/// Speed booster: on/off, then Short/Medium/Long distance multipliers.
fn booster_rows(page: &mut SettingsPage, settings: &serde_json::Value, keys: [&str; 4]) {
    page.row(vec![with_hint(
        toggle(settings, keys[0], "Speed booster"),
        "Scale the pull by distance: short < 30 px, medium 30–80 px, long > 80 px",
    )]);
    if is_on(settings, keys[0]) {
        let range = (0.0, 3.0, 0.05);
        page.row(vec![
            slider(settings, keys[1], "Short", range, "×"),
            slider(settings, keys[2], "Medium", range, "×"),
            slider(settings, keys[3], "Long", range, "×"),
        ]);
    }
}

/// Object Detection runtimes: ONNX Runtime on CPU or a GPU execution
/// provider, and OpenVINO (CPU/GPU/NPU).
#[cfg(not(target_os = "macos"))]
const MODEL_BACKENDS: &[(&str, &str)] = &[
    ("ONNX Runtime · CPU", "onnxruntime"),
    ("ONNX Runtime · DirectML", "directml"),
    ("ONNX Runtime · CUDA", "cuda"),
    ("TensorRT", "tensorrt"),
    ("OpenVINO", "openvino"),
];
#[cfg(target_os = "macos")]
const MODEL_BACKENDS: &[(&str, &str)] = &[
    ("ONNX Runtime · CPU", "onnxruntime"),
    ("ONNX Runtime · CoreML", "coreml"),
    ("OpenVINO", "openvino"),
];
#[cfg(not(target_os = "macos"))]
const MODEL_BACKEND_VALUES: [&str; 5] = ["onnxruntime", "directml", "cuda", "tensorrt", "openvino"];
#[cfg(target_os = "macos")]
const MODEL_BACKEND_VALUES: [&str; 3] = ["onnxruntime", "coreml", "openvino"];

fn engine_is_object_detection(settings: &serde_json::Value) -> bool {
    settings.get("strategy").and_then(serde_json::Value::as_str) == Some("object_detection")
}

fn profile_name_of(settings: &serde_json::Value) -> String {
    settings
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
        .to_owned()
}

#[cfg(test)]
fn settings_fields(settings: &serde_json::Value) -> Vec<ConfigField> {
    settings_page(settings, &[])
        .into_iter()
        .flat_map(|page| page.fields)
        .collect()
}

/// Shows `settings` in the window: the rail, the current page's rows and
/// the profile being edited.
fn show_settings(app: &SettingsWindow, settings: &serde_json::Value) {
    let models = BUILTIN_MODELS.with(|models| models.borrow().clone());
    let mut pages = settings_page(settings, &models);
    let current = usize::try_from(app.get_current_page())
        .unwrap_or(0)
        .min(pages.len().saturating_sub(1));
    app.set_current_page(current as i32);
    app.set_sections(ModelRc::new(VecModel::from(
        pages
            .iter()
            .map(|page| page.section.clone())
            .collect::<Vec<_>>(),
    )));
    if current < pages.len() {
        let page = pages.swap_remove(current);
        app.set_fields(ModelRc::new(VecModel::from(page.fields)));
        app.set_content_height(page.height);
    }
    app.set_profile_name(profile_name_of(settings).into());
}

thread_local! {
    /// Object Detection models built into this SysX, from the backend.
    static BUILTIN_MODELS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// Field labels for messages, instead of storage keys.
fn setting_label(key: &str) -> &str {
    match key {
        "modelConfidence" => "Min confidence",
        "abilityConfidence" => "Min ability confidence",
        "continueInsideBox" => "No deadzone",
        "fovDegrees" => "FOV",
        "dynamicFovDegrees" => "Dynamic FOV size",
        "cropResolution" => "Crop resolution",
        "speedNear" => "Booster short",
        "speedMid" => "Booster medium",
        "speedFar" => "Booster long",
        "motionSpeedX" => "Speed X",
        "motionSpeedY" => "Speed Y",
        "smoothFovDegrees" => "Smooth FOV",
        "smoothingXMin" => "X smooth min",
        "smoothingXMax" => "X smooth max",
        "smoothingYMin" => "Y smooth min",
        "smoothingYMax" => "Y smooth max",
        "offsetX" => "X offset",
        "offsetY" => "Y offset",
        "sens" => "In-game sensitivity",
        "magnet.fovDegrees" => "Magnet FOV",
        "magnet.dynamicFovDegrees" => "Magnet dynamic FOV size",
        "magnet.offsetX" => "Magnet X offset",
        "magnet.offsetY" => "Magnet Y offset",
        "magnet.delayMs" => "Magnet delay",
        "magnet.cooldownMs" => "Magnet cooldown",
        "magnet.sleepMs" => "Magnet sleep",
        "magnet.forcedReleaseMs" => "Magnet max burst",
        "magnet.smoothXMin" => "Magnet X smooth min",
        "magnet.smoothXMax" => "Magnet X smooth max",
        "magnet.smoothYMin" => "Magnet Y smooth min",
        "magnet.smoothYMax" => "Magnet Y smooth max",
        "magnet.speedNear" => "Magnet booster short",
        "magnet.speedMid" => "Magnet booster medium",
        "magnet.speedFar" => "Magnet booster long",
        "recoil.recoilLevels" => "Recoil pattern",
        "recoil.recoilPercentage" => "Recoil reduction factor",
        "recoil.recoilStartDelayMs" => "Recoil start delay",
        "recoil.recoilLevelDurationMs" => "Recoil time per bullet",
        "port" => "UDP input port",
        other => other,
    }
}

/// Status line under the page; its tone follows the message.
/// Unsaved and invalid markers shown on the page.
fn sync_flags(app: &SettingsWindow, dirty: bool, invalid: &HashSet<String>) {
    app.set_dirty(dirty);
    app.set_invalid_key(invalid.iter().min().cloned().unwrap_or_default().into());
}

fn set_status(app: &SettingsWindow, message: slint::SharedString) {
    let kind = if message.is_empty() {
        ""
    } else if message == "Changes applied successfully." {
        "success"
    } else if message.ends_with('…') {
        "info"
    } else {
        "error"
    };
    app.set_message(message);
    app.set_message_kind(kind.into());
}

/// The other end of a Min/Max smoothing range, and whether `key` is the Min.
fn smoothing_partner(key: &str) -> Option<(String, bool)> {
    let (stem, is_min) = if let Some(stem) = key.strip_suffix("Min") {
        (stem, true)
    } else {
        (key.strip_suffix("Max")?, false)
    };
    stem.to_ascii_lowercase().contains("smooth").then(|| {
        (
            format!("{stem}{}", if is_min { "Max" } else { "Min" }),
            is_min,
        )
    })
}

fn update_setting(settings: &mut serde_json::Value, key: &str, input: &str) -> Result<(), String> {
    update_one_setting(settings, key, input)?;
    // Motion runs while the trigger is held unless Always on or Toggle is
    // on; switching one on switches the other off.
    let other = match key {
        "motionAlwaysOn" => Some("motionToggle"),
        "motionToggle" => Some("motionAlwaysOn"),
        _ => None,
    };
    if let Some(other) = other {
        if input == "true" {
            update_one_setting(settings, other, "false")?;
        }
    }
    // Dragging Min past Max (or Max under Min) moves the other end along.
    if let Some((partner, is_min)) = smoothing_partner(key) {
        let read = |settings: &serde_json::Value, key: &str| {
            setting_text(settings, key).parse::<f64>().ok()
        };
        if let (Some(value), Some(other)) = (read(settings, key), read(settings, &partner)) {
            if (is_min && value > other) || (!is_min && value < other) {
                update_one_setting(settings, &partner, input)?;
            }
        }
    }
    Ok(())
}

fn update_one_setting(
    settings: &mut serde_json::Value,
    key: &str,
    input: &str,
) -> Result<(), String> {
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
    let name = setting_label(key);
    *current = if current.is_boolean() {
        serde_json::Value::Bool(
            input
                .parse()
                .map_err(|_| format!("{name} must be true or false"))?,
        )
    } else if current.is_number() {
        if current.as_u64().is_some() {
            serde_json::json!(input
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a whole number"))?)
        } else if current.as_i64().is_some() {
            serde_json::json!(input
                .parse::<i64>()
                .map_err(|_| format!("{name} must be a whole number"))?)
        } else {
            let number = input
                .parse::<f64>()
                .map_err(|_| format!("{name} must be a number"))?;
            if !number.is_finite() {
                return Err(format!("{name} must be finite"));
            }
            if whole_number_key(key) {
                serde_json::json!(number.round())
            } else {
                serde_json::json!(number)
            }
        }
    } else if current.is_array() && key == "recoil.recoilLevels" {
        serde_json::json!(sysx_ui::settings::parse_recoil_levels(input).map_err(str::to_owned)?)
    } else {
        serde_json::Value::String(input.to_owned())
    };
    Ok(())
}

fn activate_setting(settings: &mut serde_json::Value, key: &str) -> Result<(), String> {
    if key == "activeModelId" || key == "activeModelId.prev" {
        let models = BUILTIN_MODELS.with(|models| models.borrow().clone());
        let current = settings
            .get("activeModelId")
            .and_then(serde_json::Value::as_str);
        let step = if key == "activeModelId" {
            1
        } else {
            models.len().saturating_sub(1)
        };
        let next = models
            .iter()
            .position(|(id, _)| Some(id.as_str()) == current)
            .map(|index| &models[(index + step) % models.len()].0)
            .or(models.first().map(|(id, _)| id))
            .ok_or_else(|| "This build has no built-in model".to_owned())?;
        settings["activeModelId"] = serde_json::json!(next);
        return Ok(());
    }
    let current = key
        .split('.')
        .try_fold(&*settings, |value, part| value.get(part));
    let choices: Vec<&str> = match key {
        "strategy" => vec!["pixel_seek", "object_detection"],
        "modelBackend" => MODEL_BACKEND_VALUES.to_vec(),
        "pixelColor"
        | "scale"
        | "triggerButton"
        | "magnet.triggerButton"
        | "dynamicFovKey"
        | "magnet.dynamicFovKey"
        | "computeTarget"
        | "modelPrecision"
        | "colorSwap"
        | "magnet.mode"
        | "recoil.recoilPattern" => choice_options(key)
            .iter()
            .map(|(_, value)| *value)
            .collect(),
        "colorFilter"
        | "motionEnabled"
        | "motionAlwaysOn"
        | "motionToggle"
        | "speedBooster"
        | "continueInsideBox"
        | "motionRenderer.enabled"
        | "magnet.speedBooster"
        | "magnet.enabled"
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
    app.set_motion_toggle(
        state
            .filter(|s| s.motion_enabled && s.motion_toggle && !s.motion_always_on)
            .map(|s| if s.motion_toggled { "ON" } else { "OFF" })
            .unwrap_or("")
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
    // A panic in one refresh must not close the window; the next refresh
    // draws again.
    let _ = catch_refresh(|| refresh(app, view, last));
}

/// The ring marks the point Motion aims at in both engines: the targeted
/// pixel for Pixel Seek, the box center for Object Detection (user request).
fn target_point(meta: &sysx_ui::protocol::LiveFrame) -> Option<[f32; 2]> {
    meta.detection.target_xy
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
        // Corners only, no full outline (user request, 2026-10-26); a dark
        // underlay keeps them readable on bright frames.
        let c = (w.min(h) * 0.22).clamp(7., 16.).min(w / 2.).min(h / 2.);
        let corners = format!(
            "M0 {c}V0H{c} M{} 0H{w}V{c} M{w} {}V{h}H{} M{c} {h}H0V{}",
            w - c,
            h - c,
            w - c,
            h - c
        );
        let _ = write!(
            svg,
            r##"<g fill="none" stroke-linejoin="round" transform="translate({x},{y})"><path d="{corners}" stroke="#05090a" stroke-opacity=".88" stroke-width="5"/><path d="{corners}" stroke="{stroke}" stroke-width="3"/></g>"##
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
    // Auto has no target.
    if meta.detection.auto_active {
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
    // Segoe UI and Consolas (the markup defaults) are Windows fonts; macOS
    // would fall back to a proportional face for the measured numbers.
    #[cfg(target_os = "macos")]
    {
        app.set_body_font("Helvetica Neue".into());
        app.set_mono_font("Menlo".into());
        settings_app.set_body_font("Helvetica Neue".into());
        settings_app.set_mono_font("Menlo".into());
    }
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
    let recover_gate = Arc::clone(&awaiting);
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
    // Set before every quit_event_loop(): any other end of the event loop is
    // a render or window error to recover from, not a request to exit.
    let quit_requested = Rc::new(Cell::new(false));
    // The backend went away (it crashed or was killed): SysX did not end
    // because the user closed it.
    let backend_lost = Rc::new(Cell::new(false));
    let backend_lost_events = Rc::clone(&backend_lost);
    let monitor_hidden = Rc::clone(&hidden);
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
                    set_status(&settings, "Loading settings…".into());
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
    settings_app.on_page_selected({
        let draft = Rc::clone(&settings_draft);
        let weak = settings_app.as_weak();
        move |_| {
            if let (Some(app), Some(settings)) = (weak.upgrade(), draft.borrow().as_ref()) {
                show_settings(&app, settings);
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
                sync_flags(&app, dirty.get(), &invalid.borrow());
                set_status(&app, result.err().unwrap_or_default().into());
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
                sync_flags(&app, dirty.get(), &invalid.borrow());
                if key != "profile.select" {
                    if let Some(settings) = draft.borrow().as_ref() {
                        show_settings(&app, settings);
                    }
                }
                set_status(
                    &app,
                    if key == "profile.select" {
                        result
                            .map(|_| "Updating profile…".to_owned())
                            .unwrap_or_else(|error| error)
                            .into()
                    } else {
                        result.err().unwrap_or_default().into()
                    },
                );
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
                    set_status(&app, result.map(|_| "Updating profile…".to_owned()).unwrap_or_else(|error| error).into());
                }
                return;
            }
            let result = draft
                .borrow_mut()
                .as_mut()
                .ok_or_else(|| "Settings have not loaded yet".to_owned())
                .and_then(|settings| {
                    if let Some(key) = invalid.borrow().iter().next() {
                        Err(format!("Fix the invalid value for {} before applying.", setting_label(key)))
                    } else {
                        activate_setting(settings, &key)
                    }
                });
            if result.is_ok() {
                dirty.set(true);
                invalid.borrow_mut().remove(key.as_str());
            }
            if let Some(app) = weak.upgrade() {
                sync_flags(&app, dirty.get(), &invalid.borrow());
                match result {
                    Ok(()) => {
                        if let Some(settings) = draft.borrow().as_ref() {
                            show_settings(&app, settings);
                        }
                        set_status(&app, "".into());
                    }
                    Err(error) => set_status(&app, error.into()),
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
                        Err(format!(
                            "Fix the invalid value for {} before applying.",
                            setting_label(key)
                        ))
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
                set_status(
                    &app,
                    result.err().unwrap_or_else(|| "Applying…".into()).into(),
                );
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
    let quit_events = Rc::clone(&quit_requested);
    let settings_weak_events = settings_app.as_weak();
    let settings_commands_events = commands.clone();
    let mut apply_success_until = None::<Instant>;
    let mut reloaded_for = None::<u8>;
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
                            // The reload right after Apply keeps its success message.
                            let keep_status = apply_success_until.is_some();
                            if let Some(models) = value.get("builtin_models") {
                                // Each entry: {id, label, variants}.
                                let models = models
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|model| {
                                        let id = model.get("id")?.as_str()?;
                                        let label = model
                                            .get("label")
                                            .and_then(serde_json::Value::as_str)
                                            .unwrap_or(id);
                                        Some((id.to_owned(), label.to_owned()))
                                    })
                                    .collect();
                                BUILTIN_MODELS.with(|list| *list.borrow_mut() = models);
                            }
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
                                    show_settings(&app, settings);
                                    sync_flags(&app, false, &HashSet::new());
                                    if !keep_status {
                                        set_status(&app, "".into());
                                    }
                                }
                            }
                        } else if command_type == Some("apply_app_settings")
                            || value.get("request_id").and_then(|id| id.as_str())
                                == Some("native-settings")
                        {
                            if let Some(app) = settings_weak_events.upgrade() {
                                if event == Some("ack") {
                                    settings_dirty_events.set(false);
                                    invalid_settings_events.borrow_mut().clear();
                                    // Reload: the backend keeps a profile the
                                    // middle button selected after this draft.
                                    let _ = settings_commands_events.try_send(serde_json::json!({
                                        "version":1,"type":"get_app_settings"
                                    }));
                                    apply_success_until =
                                        Some(Instant::now() + Duration::from_secs(3));
                                    sync_flags(&app, false, &HashSet::new());
                                    set_status(&app, "Changes applied successfully.".into());
                                } else {
                                    apply_success_until = None;
                                    set_status(
                                        &app,
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
                                set_status(
                                    &app,
                                    value
                                        .get("message")
                                        .and_then(|message| message.as_str())
                                        .unwrap_or("Profile action failed.")
                                        .into(),
                                );
                            }
                        } else if event == Some("error") {
                        }
                    }
                    Ok(crate::native_app::HostEvent::Disconnected)
                    | Err(TryRecvError::Disconnected) => {
                        backend_lost_events.set(true);
                        quit_events.set(true);
                        let _ = slint::quit_event_loop();
                        return;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            // The middle button cycled profiles: reload an unedited Settings
            // draft so it shows (and later applies) the active profile.
            if settings_visible_events.get() && !settings_dirty_events.get() {
                let active = view
                    .lock()
                    .ok()
                    .and_then(|view| view.state.as_ref().map(|state| state.profile_index));
                let draft_active = settings_draft_events
                    .borrow()
                    .as_ref()
                    .and_then(|settings| settings.get("activeProfile")?.as_u64());
                if let (Some(active), Some(draft_active)) = (active, draft_active) {
                    if u64::from(active) == draft_active {
                        reloaded_for = None;
                    } else if reloaded_for != Some(active) {
                        reloaded_for = Some(active);
                        let _ = settings_commands_events.try_send(serde_json::json!({
                            "version":1,"type":"get_app_settings"
                        }));
                    }
                }
            }
            if apply_success_until.is_some_and(|deadline| Instant::now() >= deadline) {
                apply_success_until = None;
                if let Some(settings_app) = settings_weak_events.upgrade() {
                    if settings_app.get_message().as_str() == "Changes applied successfully." {
                        set_status(&settings_app, "".into());
                    }
                }
            }
            if signal.requested() {
                if let Some(app) = weak.upgrade() {
                    if hidden.replace(false) {
                        if let Ok(next) =
                            Workers::start(port, token.clone(), Arc::clone(&view), notify.clone())
                        {
                            *workers.borrow_mut() = Some(next);
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
                quit_events.set(true);
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
    let mut failures = std::collections::VecDeque::new();
    loop {
        let error = match slint::run_event_loop_until_quit() {
            Ok(()) if backend_lost.get() => return Err("backend connection lost".into()),
            Ok(()) => return Ok(()),
            Err(error) if quit_requested.get() => return Err(error.to_string()),
            Err(error) => error,
        };
        // A draw, resize or focus error (the OpenGL context lost to a
        // driver reset, a game switching fullscreen or display mode, sleep)
        // ends Slint's event loop. That used to close SysX mid-use without a
        // crash. Rebuild the visible windows and keep running; give up only
        // if it keeps failing.
        let now = Instant::now();
        failures.retain(|at| now.duration_since(*at) < Duration::from_secs(10));
        failures.push_back(now);
        if failures.len() > MAX_EVENT_LOOP_FAILURES {
            return Err(error.to_string());
        }
        recover_window(app.window(), !monitor_hidden.get());
        recover_window(settings_app.window(), settings_visible.get());
        // The failed draw never reported a finished frame; let the decoder
        // go on instead of waiting for it.
        recover_gate.mark_drawn();
    }
}

/// Event loop errors tolerated within 10 s before SysX gives up.
const MAX_EVENT_LOOP_FAILURES: usize = 5;

/// Destroy and recreate a shown window, so it gets a new native window and
/// graphics context in place of the one that failed.
fn recover_window(window: &slint::Window, visible: bool) {
    if !visible {
        return;
    }
    // On Windows, hide() only hides the native window unless this is set;
    // with it, the window and its OpenGL context are dropped.
    std::env::set_var("SLINT_DESTROY_WINDOW_ON_HIDE", "1");
    let _ = window.hide();
    std::env::remove_var("SLINT_DESTROY_WINDOW_ON_HIDE");
    let _ = window.show();
    window.request_redraw();
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
    fn object_detection_shows_model_controls_instead_of_target_color() {
        let fields = super::settings_fields(&serde_json::json!({"strategy":"pixel_seek"}));
        let engine = fields.iter().find(|field| field.key == "strategy").unwrap();
        assert_eq!(engine.kind, "engine");
        assert_eq!(engine.value.as_str(), "pixel_seek");
        assert!(fields.iter().any(|field| field.key == "pixelColor"));
        assert!(!fields.iter().any(|field| field.key == "activeModelId"));
        let fields = super::settings_fields(&serde_json::json!({
            "strategy":"object_detection","activeModelId":"yolo",
            "modelConfidence":"0.5","modelBackend":"onnxruntime","computeTarget":"cpu"
        }));
        let field = |key| fields.iter().find(|field| field.key == key);
        assert!(field("pixelColor").is_some(), "the color filter's color");
        assert_eq!(field("activeModelId").unwrap().kind.as_str(), "info");
        let models = [
            ("light".to_owned(), "Light (256×256)".to_owned()),
            ("heavy".to_owned(), "Heavy (640×640)".to_owned()),
        ];
        let with_models = super::settings_page(
            &serde_json::json!({"strategy":"object_detection","activeModelId":"heavy"}),
            &models,
        );
        let model = with_models
            .iter()
            .flat_map(|page| page.fields.iter())
            .find(|field| field.key == "activeModelId")
            .unwrap();
        assert_eq!(
            (model.kind.as_str(), model.value.as_str()),
            ("dropdown", "heavy")
        );
        assert_eq!(model.choices.row_count(), 2);
        assert_eq!(model.value2.as_str(), "Heavy (640×640)", "shown by label");
        super::BUILTIN_MODELS.with(|list| *list.borrow_mut() = models.to_vec());
        let mut draft = serde_json::json!({"activeModelId":"heavy"});
        super::activate_setting(&mut draft, "activeModelId").unwrap();
        assert_eq!(draft["activeModelId"], "light", "› wraps around");
        super::activate_setting(&mut draft, "activeModelId.prev").unwrap();
        assert_eq!(draft["activeModelId"], "heavy");
        assert_eq!(field("modelConfidence").unwrap().value.as_str(), "0.5");
        assert_eq!(
            field("modelBackend").unwrap().choices.row_count(),
            super::MODEL_BACKENDS.len()
        );
        assert!(
            field("computeTarget").is_none(),
            "only OpenVINO picks a device"
        );
        let fields = super::settings_fields(&serde_json::json!({
            "strategy":"object_detection","modelBackend":"openvino","computeTarget":"gpu"
        }));
        let device = fields
            .iter()
            .find(|field| field.key == "computeTarget")
            .unwrap();
        assert_eq!(device.choices.row_count(), 3);
        let fields = super::settings_fields(&serde_json::json!({}));
        let sections = fields
            .iter()
            .filter(|field| field.kind == "section")
            .map(|field| field.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            sections,
            [
                "Profiles",
                "Detection",
                "Motion",
                "Magnet",
                "Recoil",
                "Connection"
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
            "auto":{"triggerButton":"m4"}
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
        assert_eq!(field("profile.actions").choices.row_count(), 3);
        assert_eq!(
            field("profile.rename").key2.as_str(),
            "profile.rename.apply"
        );
    }

    #[test]
    fn each_settings_section_is_its_own_page_laid_out_as_a_grid() {
        let settings = serde_json::json!({
            "profiles":[{"name":"Default"}],"activeProfile":0,
            "speedBooster":true,"magnet":{"mode":"spray","speedBooster":true}
        });
        let pages = super::settings_page(&settings, &[]);
        let labels: Vec<_> = pages
            .iter()
            .map(|page| page.section.label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "Profiles",
                "Detection",
                "Motion",
                "Magnet",
                "Recoil",
                "Connection"
            ]
        );
        for page in &pages {
            let (heading, cells) = page.fields.split_first().unwrap();
            assert_eq!(heading.kind, "section");
            assert_eq!(heading.y, super::SETTINGS_TOP, "{}", heading.label);
            assert!(!cells.is_empty(), "{}", heading.label);
            assert_eq!(
                cells[0].y,
                super::SETTINGS_TOP + super::SETTINGS_HEADING + super::CARD_PAD
            );
            let mut previous_bottom = 0.0_f32;
            let mut row_y = f32::NAN;
            for cell in cells {
                assert!(cell.kind != "section");
                assert!(
                    cell.x >= 0.0 && cell.x + cell.w <= 1.0 + 1e-6,
                    "{}",
                    cell.label
                );
                assert_eq!(cell.h, super::cell_height(cell), "{}", cell.label);
                if cell.y != row_y {
                    assert!(
                        cell.y >= previous_bottom,
                        "{}: rows never overlap",
                        cell.label
                    );
                    row_y = cell.y;
                }
                previous_bottom = previous_bottom.max(cell.y + cell.h);
            }
            // The card closes under the last row; the page adds its margin.
            assert_eq!(heading.y + heading.h, previous_bottom + super::CARD_PAD);
            assert_eq!(
                page.height,
                previous_bottom + super::CARD_PAD + super::SETTINGS_TOP
            );
        }
        let all: Vec<_> = pages.iter().flat_map(|page| page.fields.iter()).collect();
        let find = |key: &str| all.iter().filter(|cell| cell.key == key).count();
        // Short / Medium / Long boosters for both Motion and Magnet.
        for key in [
            "speedNear",
            "speedMid",
            "speedFar",
            "magnet.speedNear",
            "magnet.speedFar",
        ] {
            assert_eq!(find(key), 1, "{key}");
        }
        let mode = all.iter().find(|cell| cell.key == "magnet.mode").unwrap();
        assert_eq!(mode.kind, "dropdown");
        assert!(mode.choices.iter().any(|choice| choice.value == "auto"));
        // Auto has no smoothing or booster.
        let auto = super::settings_page(&serde_json::json!({"magnet":{"mode":"auto"}}), &[]);
        let auto: Vec<_> = auto.iter().flat_map(|page| page.fields.iter()).collect();
        assert!(!auto
            .iter()
            .any(|cell| cell.key.starts_with("magnet.smooth")));
        assert!(!auto.iter().any(|cell| cell.key == "magnet.speedBooster"));
        assert!(auto.iter().any(|cell| cell.key == "magnet.delayMs"));
    }

    #[test]
    fn a_smoothing_min_past_its_max_moves_the_max_along() {
        let mut settings = serde_json::json!({
            "smoothingXMin":5.0,"smoothingXMax":8.0,
            "magnet":{"smoothYMin":4.0,"smoothYMax":6.0}
        });
        // Whole numbers: 9.5 becomes 10.
        super::update_setting(&mut settings, "smoothingXMin", "9.5").unwrap();
        assert_eq!(settings["smoothingXMin"], 10.0);
        assert_eq!(settings["smoothingXMax"], 10.0);
        assert_eq!(super::setting_text(&settings, "smoothingXMax"), "10");
        super::update_setting(&mut settings, "magnet.smoothYMax", "3").unwrap();
        assert_eq!(settings["magnet"]["smoothYMin"], 3.0);
        super::update_setting(&mut settings, "smoothingXMin", "2").unwrap();
        assert_eq!(
            settings["smoothingXMax"], 10.0,
            "inside the range: unchanged"
        );
    }

    #[test]
    fn always_on_and_toggle_switch_each_other_off_and_offsets_are_whole() {
        let mut settings = serde_json::json!({
            "motionAlwaysOn":false,"motionToggle":false,"offsetX":0.0,"speedNear":1.0
        });
        super::update_setting(&mut settings, "motionToggle", "true").unwrap();
        super::update_setting(&mut settings, "motionAlwaysOn", "true").unwrap();
        assert_eq!((settings["motionAlwaysOn"].as_bool(), settings["motionToggle"].as_bool()), (Some(true), Some(false)));
        super::update_setting(&mut settings, "motionToggle", "true").unwrap();
        assert_eq!((settings["motionAlwaysOn"].as_bool(), settings["motionToggle"].as_bool()), (Some(false), Some(true)));
        // Turning one off leaves the other alone (both off = hold the trigger).
        super::update_setting(&mut settings, "motionToggle", "false").unwrap();
        assert_eq!(settings["motionAlwaysOn"], false);
        super::update_setting(&mut settings, "offsetX", "-2.6").unwrap();
        assert_eq!(super::setting_text(&settings, "offsetX"), "-3");
        super::update_setting(&mut settings, "speedNear", "1.25").unwrap();
        assert_eq!(settings["speedNear"], 1.25, "the booster keeps decimals");
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
        assert!(!svg.contains("<path"), "no box: {svg}");
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
        let boxes = super::overlay_svg(&meta, rect, 640., "red");
        assert!(boxes.contains("<path"), "{boxes}");
        assert!(
            !boxes.contains("<rect"),
            "corners only, no outline: {boxes}"
        );
        // Object Detection rings the same Motion target, not the smoothed point.
        let layer = super::target_layer(&meta, rect, "red").unwrap();
        assert!(layer.svg.contains("cx=\"480\" cy=\"280\""), "{}", layer.svg);
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

        window.set_size(slint::PhysicalSize::new(940, 1000));
        let settings = SettingsWindow::new().unwrap();
        if !cfg!(windows) {
            settings.set_body_font("DejaVu Sans".into());
            settings.set_mono_font("DejaVu Sans Mono".into());
        }
        let settings_json: serde_json::Value = serde_json::from_str(
            r#"{
            "profiles":[{"name":"Default"},{"name":"Low recoil"}],"activeProfile":0,
            "pixelColor":"yellow","fovDegrees":90,"colorFilter":false,
            "cropResolution":256,"scale":"Smooth","motionEnabled":true,
            "motionAlwaysOn":false,"motionToggle":false,"triggerButton":"m4",
            "dynamicFovDegrees":160,"dynamicFovKey":"right",
            "speedBooster":true,"speedNear":0.8,"speedMid":1.25,"speedFar":1.5,
            "continueInsideBox":false,"abilityConfidence":0.6,"motionSpeedX":0.045,"motionSpeedY":0.045,"smoothFovDegrees":20,"colorSwap":"yellow",
            "offsetX":0,"offsetY":4,"smoothingXMin":5,"smoothingXMax":6,
            "smoothingYMin":6,"smoothingYMax":8,"sens":0.35,
            "modelPrecision":"fp16",
            "motionRenderer":{"enabled":false},
            "recoil":{"recoilEnabled":true,"recoilPattern":"custom","recoilLevels":[1,2,3,3,4,3,4,5,5,7],
                "recoilPercentage":75,"recoilStartDelayMs":130,"recoilLevelDurationMs":50},
            "magnet":{"enabled":true,"mode":"tap","triggerButton":"m5","fovDegrees":120,
                "dynamicFovDegrees":160,"dynamicFovKey":"right","offsetX":0,"offsetY":2,
                "delayMs":30,"cooldownMs":120,"sleepMs":0,"fireHoldMs":30,"forcedReleaseMs":250,
                "smoothXMin":4,"smoothXMax":5.9,
                "smoothYMin":10.7,"smoothYMax":12.5,"speedBooster":true,
                "speedNear":1.0,"speedMid":1.2,"speedFar":1.6},
            "port":4200
        }"#,
        )
        .unwrap();
        show_settings(&settings, &settings_json);
        settings.set_dirty(true);
        settings.show().unwrap();
        settings.invoke_reset_scroll();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 940 * 1000];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 940);
        });
        assert!(pixels.iter().any(|p| p.r != 0 || p.g != 0 || p.b != 0));
        if let Ok(dir) = std::env::var("SYSX_NATIVE_CAPTURE_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            let raw = pixels
                .iter()
                .flat_map(|p| [p.r, p.g, p.b])
                .collect::<Vec<_>>();
            image::RgbImage::from_raw(940, 1000, raw)
                .unwrap()
                .save(std::path::Path::new(&dir).join("settings-native.png"))
                .unwrap();
        }
        // Every page, for reviewing each section.
        let json = settings_json.clone();
        for page in 0..slint::Model::row_count(&settings.get_sections()) {
            settings.set_current_page(page as i32);
            show_settings(&settings, &json);
            assert_eq!(settings.get_current_page(), page as i32);
            let mut pixels = vec![slint::Rgb8Pixel::default(); 940 * 1000];
            window.request_redraw();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 940);
            });
            if let Ok(dir) = std::env::var("SYSX_NATIVE_CAPTURE_DIR") {
                let raw = pixels
                    .iter()
                    .flat_map(|p| [p.r, p.g, p.b])
                    .collect::<Vec<_>>();
                image::RgbImage::from_raw(940, 1000, raw)
                    .unwrap()
                    .save(std::path::Path::new(&dir).join(format!("settings-page-{page}.png")))
                    .unwrap();
            }
        }
        // Detection page with the Object Detection model and runtime rows.
        let mut json = settings_json.clone();
        json["strategy"] = "object_detection".into();
        json["activeModelId"] = "light_256".into();
        super::BUILTIN_MODELS.with(|list| {
            *list.borrow_mut() = vec![
                ("light_256".into(), "Light (256×256)".into()),
                ("heavy_640".into(), "Heavy (640×640)".into()),
            ]
        });
        json["modelConfidence"] = "0.5".into();
        json["modelBackend"] = "openvino".into();
        json["computeTarget"] = "gpu".into();
        settings.set_current_page(1);
        show_settings(&settings, &json);
        let mut pixels = vec![slint::Rgb8Pixel::default(); 940 * 1000];
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 940);
        });
        if let Ok(dir) = std::env::var("SYSX_NATIVE_CAPTURE_DIR") {
            let raw = pixels
                .iter()
                .flat_map(|p| [p.r, p.g, p.b])
                .collect::<Vec<_>>();
            image::RgbImage::from_raw(940, 1000, raw)
                .unwrap()
                .save(std::path::Path::new(&dir).join("settings-object-detection.png"))
                .unwrap();
        }
        settings.hide().unwrap();
    }
}
