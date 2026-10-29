//! [`PaletteView`]: the overlay over [`PaletteModel`]. A search field, the ranked commands with
//! their shortcuts, and keyboard selection. It never runs a command itself: it emits
//! [`PaletteEvent::Run`] and the shell does it.

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, *};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::model::{Command, CommandId, PaletteModel};
use super::{CONTEXT, ClosePalette, SelectNextCommand, SelectPreviousCommand, label, tr, tr_args};

/// Element ids, for tests and the accessibility tree.
pub const OVERLAY_ID: &str = "palette";
pub const INPUT_ID: &str = "palette-input";
pub const LIST_ID: &str = "palette-list";
pub const EMPTY_ID: &str = "palette-empty";
/// The no-match state's action: empty the search field and show every command again.
pub const EMPTY_CLEAR_ID: &str = "palette-empty-action";
/// Id of row `ix`: `palette-item-<ix>`, best match first.
pub fn item_id(ix: usize) -> String {
    format!("palette-item-{ix}")
}

/// What the palette tells the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEvent {
    /// The user chose this command (the palette has already closed).
    Run(CommandId),
    /// The palette closed without running anything.
    Closed,
}

pub struct PaletteView {
    model: PaletteModel,
    input: Entity<InputState>,
    open: bool,
    /// What had focus when the palette opened; gets it back when the palette closes.
    previous_focus: Option<FocusHandle>,
    scroll: ScrollHandle,
    _input_events: Subscription,
}

impl EventEmitter<PaletteEvent> for PaletteView {}

impl PaletteView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("palette-input-placeholder")));
        let input_events = cx.subscribe_in(&input, window, Self::on_input_event);
        Self {
            model: PaletteModel::default(),
            input,
            open: false,
            previous_focus: None,
            scroll: ScrollHandle::new(),
            _input_events: input_events,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn model(&self) -> &PaletteModel {
        &self.model
    }

    /// The search field.
    pub fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// Show the palette over `commands` with an empty query and the field focused.
    pub fn open(&mut self, commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            self.previous_focus = window.focused(cx);
        }
        self.model = PaletteModel::new(commands);
        self.open = true;
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let field = self.input.focus_handle(cx);
        window.focus(&field, cx);
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    /// Empty the search field (the no-match state's action) and put the cursor back in it.
    pub fn clear_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        // Setting the value does not announce a change, so the model is told here.
        self.model.set_query("");
        self.scroll.scroll_to_item(0);
        let field = self.input.focus_handle(cx);
        window.focus(&field, cx);
        cx.notify();
    }

    /// Hide the palette and give focus back to where it was.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismiss(window, cx) {
            cx.emit(PaletteEvent::Closed);
        }
    }

    /// Close without announcing it; `false` when it was not open.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.open {
            return false;
        }
        self.open = false;
        if let Some(previous) = self.previous_focus.take() {
            window.focus(&previous, cx);
        }
        cx.notify();
        true
    }

    /// Run the selected command: close, then tell the shell. Nothing happens with no matches.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self.model.selected().map(|command| command.id.clone()) else {
            return;
        };
        self.run(command, window, cx);
    }

    fn run(&mut self, command: CommandId, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismiss(window, cx) {
            cx.emit(PaletteEvent::Run(command));
        }
    }

    fn select_next(&mut self, cx: &mut Context<Self>) {
        self.model.select_next();
        self.scroll.scroll_to_item(self.model.selected_index());
        cx.notify();
    }

    fn select_previous(&mut self, cx: &mut Context<Self>) {
        self.model.select_previous();
        self.scroll.scroll_to_item(self.model.selected_index());
        cx.notify();
    }

    fn on_input_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                self.model.set_query(&text);
                self.scroll.scroll_to_item(0);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.confirm(window, cx),
            _ => {}
        }
    }

    fn render_item(
        &self,
        ix: usize,
        command: &Command,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = command.id.clone();
        let shortcut = command.shortcut.map(label);
        let a11y = match &shortcut {
            Some(shortcut) => tr_args(
                "palette-item-shortcut-a11y",
                &[("title", &command.title), ("shortcut", shortcut)],
            ),
            None => tr_args("palette-item-a11y", &[("title", &command.title)]),
        };
        let theme = cx.theme();
        let (background, foreground, muted) = if selected {
            (
                theme.accent,
                theme.accent_foreground,
                theme.accent_foreground,
            )
        } else {
            (
                theme.popover,
                theme.popover_foreground,
                theme.muted_foreground,
            )
        };
        h_flex()
            .id(SharedString::from(item_id(ix)))
            .role(Role::ListBoxOption)
            .aria_selected(selected)
            .test_support()
            .aria_label(a11y)
            .px_3()
            .py_1()
            .gap_3()
            .justify_between()
            .rounded_md()
            .bg(background)
            .text_color(foreground)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.run(id.clone(), window, cx);
            }))
            .child(div().flex_1().min_w_0().child(command.title.clone()))
            .children(shortcut.map(|shortcut| div().text_color(muted).child(shortcut)))
    }
}

impl Focusable for PaletteView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for PaletteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let matches: Vec<(usize, Command)> = self
            .model
            .matches()
            .into_iter()
            .cloned()
            .enumerate()
            .collect();
        let selected = self.model.selected_index();
        let count = matches.len().to_string();
        let items: Vec<_> = matches
            .iter()
            .map(|(ix, command)| self.render_item(*ix, command, *ix == selected, cx))
            .collect();
        let theme = cx.theme();
        div()
            .id("palette-overlay")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(96.))
            .child(
                v_flex()
                    .id(OVERLAY_ID)
                    .key_context(CONTEXT)
                    .role(Role::Dialog)
                    .test_support()
                    .aria_label(tr("palette-title"))
                    .w(px(560.))
                    .max_w_full()
                    .p_2()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .shadow_lg()
                    .on_action(cx.listener(|this, _: &ClosePalette, window, cx| {
                        this.close(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &SelectNextCommand, _, cx| {
                        this.select_next(cx);
                    }))
                    .on_action(cx.listener(|this, _: &SelectPreviousCommand, _, cx| {
                        this.select_previous(cx);
                    }))
                    .child(
                        div()
                            .id(INPUT_ID)
                            .test_support()
                            .aria_label(tr("palette-input-a11y"))
                            .child(Input::new(&self.input)),
                    )
                    .child(
                        v_flex()
                            .id(LIST_ID)
                            .role(Role::ListBox)
                            .test_support()
                            .aria_label(tr_args("palette-count-a11y", &[("count", &count)]))
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .gap_1()
                            .children(items),
                    )
                    .when(matches.is_empty(), |el| {
                        el.child(
                            div()
                                .id(EMPTY_ID)
                                .role(Role::Status)
                                .test_support()
                                .aria_label(tr("palette-empty"))
                                .px_3()
                                .py_1()
                                .text_color(theme.muted_foreground)
                                .child(tr("palette-empty")),
                        )
                        .child(
                            div().px_3().py_1().child(
                                Button::new(EMPTY_CLEAR_ID)
                                    .primary()
                                    .label(tr("palette-empty-clear"))
                                    .accessibility_label(tr("palette-empty-clear-a11y"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.clear_query(window, cx);
                                    })),
                            ),
                        )
                    }),
            )
    }
}
