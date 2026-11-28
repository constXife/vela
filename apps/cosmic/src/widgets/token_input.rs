// SPDX-License-Identifier: MIT

//! Text input that shows committed values as removable token blocks inside the
//! field (search operators today; recipients or labels later).
//!
//! The widget is stateless about its content: the caller owns the token list
//! and the draft text and reacts to the callbacks. The wrapper only adds what a
//! plain composition of libcosmic widgets cannot do: Backspace in an empty
//! draft removes the last token, clicking empty space in the field focuses the
//! input, and the whole field shows one focus ring.

use cosmic::iced::Alignment;
use cosmic::iced::core::Renderer as _;
use cosmic::iced::core::event::Event;
use cosmic::iced::core::widget::operation::{self, Operation, focusable::Focusable};
use cosmic::iced::core::widget::{Id, Tree, tree};
use cosmic::iced::core::{
    Background, Border, Clipboard, Color, Layout, Length, Rectangle, Shell, Size, Vector, Widget,
    keyboard, layout, mouse, overlay, renderer,
};
use cosmic::{Element, Renderer, Theme, widget};

/// One block shown before the draft text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub label: String,
    /// Render with the warning palette (e.g. an operator that does not parse).
    pub warning: bool,
}

/// Builder for a token input; convert it into an [`Element`] to render.
pub struct TokenInput<'a, Message> {
    id: Id,
    placeholder: &'a str,
    value: &'a str,
    tokens: Vec<Token>,
    leading_icon: Option<&'static str>,
    on_input: Option<Box<dyn Fn(String) -> Message + 'a>>,
    on_submit: Option<Box<dyn Fn(String) -> Message + 'a>>,
    on_edit_token: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    on_remove_token: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    on_remove_last: Option<Message>,
    on_clear: Option<Message>,
}

/// A token input whose inner text field uses `id` (for focusing it).
pub fn token_input<'a, Message>(
    id: Id,
    placeholder: &'a str,
    value: &'a str,
    tokens: Vec<Token>,
) -> TokenInput<'a, Message> {
    TokenInput {
        id,
        placeholder,
        value,
        tokens,
        leading_icon: None,
        on_input: None,
        on_submit: None,
        on_edit_token: None,
        on_remove_token: None,
        on_remove_last: None,
        on_clear: None,
    }
}

impl<'a, Message> TokenInput<'a, Message> {
    pub fn leading_icon(mut self, icon_name: &'static str) -> Self {
        self.leading_icon = Some(icon_name);
        self
    }

    pub fn on_input(mut self, f: impl Fn(String) -> Message + 'a) -> Self {
        self.on_input = Some(Box::new(f));
        self
    }

    pub fn on_submit(mut self, f: impl Fn(String) -> Message + 'a) -> Self {
        self.on_submit = Some(Box::new(f));
        self
    }

    /// Clicking a token's label (e.g. to move it back into the draft).
    pub fn on_edit_token(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_edit_token = Some(Box::new(f));
        self
    }

    /// Clicking a token's close button.
    pub fn on_remove_token(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_remove_token = Some(Box::new(f));
        self
    }

    /// Backspace while the draft is empty.
    pub fn on_remove_last(mut self, message: Message) -> Self {
        self.on_remove_last = Some(message);
        self
    }

    /// Shows a clear button while there is any token or draft text.
    pub fn on_clear(mut self, message: Message) -> Self {
        self.on_clear = Some(message);
        self
    }
}

impl<'a, Message: Clone + 'static> From<TokenInput<'a, Message>> for Element<'a, Message> {
    fn from(input: TokenInput<'a, Message>) -> Self {
        let has_content = !input.tokens.is_empty() || !input.value.is_empty();

        let tokens: Vec<Element<'a, Message>> = input
            .tokens
            .iter()
            .enumerate()
            .map(|(index, token)| {
                token_block(
                    token,
                    input.on_edit_token.as_ref().map(|f| f(index)),
                    input.on_remove_token.as_ref().map(|f| f(index)),
                )
            })
            .collect();

        let placeholder = if input.tokens.is_empty() {
            input.placeholder
        } else {
            ""
        };
        let mut text = widget::text_input(placeholder, input.value)
            .id(input.id.clone())
            .style(transparent_input_style())
            .padding([4, 4])
            .width(Length::Fill);
        if let Some(f) = input.on_input {
            text = text.on_input(f);
        }
        if let Some(f) = input.on_submit {
            text = text.on_submit(f);
        }

        let mut row = widget::row::with_capacity(3)
            .spacing(6)
            .align_y(Alignment::Center);
        if let Some(icon) = input.leading_icon {
            row = row.push(widget::icon::from_name(icon).size(16));
        }
        row = row.push(Element::new(TokenFlow {
            input: text.into(),
            tokens,
            spacing: 4.0,
            min_input_width: 120.0,
        }));
        if has_content && let Some(message) = input.on_clear {
            row = row.push(
                widget::button::icon(widget::icon::from_name("edit-clear-symbolic").size(16))
                    .padding(2)
                    .on_press(message),
            );
        }

        let frame = TokenFrame {
            content: widget::container(row)
                .padding([4, 12, 4, 12])
                .width(Length::Fill)
                .into(),
            input_id: input.id,
            on_remove_last: if input.value.is_empty() {
                input.on_remove_last
            } else {
                None
            },
        };
        Element::new(frame)
    }
}

fn token_block<'a, Message: Clone + 'static>(
    token: &Token,
    on_edit: Option<Message>,
    on_remove: Option<Message>,
) -> Element<'a, Message> {
    let warning = token.warning;
    let label = widget::button::custom(widget::text::caption(token.label.clone()))
        .class(cosmic::theme::Button::Text)
        .padding([2, 0, 2, 8])
        .on_press_maybe(on_edit);
    let close = widget::button::custom(widget::icon::from_name("window-close-symbolic").size(12))
        .class(cosmic::theme::Button::Text)
        .padding([4, 6, 4, 4])
        .on_press_maybe(on_remove);
    widget::container(
        widget::row::with_capacity(2)
            .push(label)
            .push(close)
            .align_y(Alignment::Center),
    )
    .width(Length::Shrink)
    .class(cosmic::theme::Container::custom(move |theme: &Theme| {
        let cosmic = theme.cosmic();
        let base: Color = if warning {
            cosmic.warning.base.into()
        } else {
            cosmic.accent.base.into()
        };
        widget::container::Style {
            background: Some(base.scale_alpha(0.18).into()),
            border: Border {
                radius: cosmic.corner_radii.radius_xl.into(),
                width: 1.0,
                color: base.scale_alpha(0.55),
            },
            ..Default::default()
        }
    }))
    .into()
}

fn transparent_input_style() -> cosmic::theme::TextInput {
    fn appearance(theme: &Theme) -> widget::text_input::Appearance {
        let cosmic = theme.cosmic();
        let container = theme.current_container();
        let on: Color = container.on.into();
        widget::text_input::Appearance {
            background: Color::TRANSPARENT.into(),
            border_radius: cosmic.corner_radii.radius_0.into(),
            border_offset: None,
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
            label_color: cosmic.palette.neutral_9.into(),
            placeholder_color: on.scale_alpha(0.6),
            selected_text_color: cosmic.on_accent_color().into(),
            icon_color: None,
            text_color: None,
            selected_fill: cosmic.accent_color().into(),
        }
    }
    cosmic::theme::TextInput::Custom {
        active: Box::new(appearance),
        error: Box::new(appearance),
        hovered: Box::new(appearance),
        focused: Box::new(appearance),
        disabled: Box::new(appearance),
    }
}

/// Wrapper that draws the field frame and handles focus-related input.
struct TokenFrame<'a, Message> {
    content: Element<'a, Message>,
    input_id: Id,
    on_remove_last: Option<Message>,
}

#[derive(Default)]
struct FrameState {
    focused: bool,
}

impl<Message: Clone> TokenFrame<'_, Message> {
    fn content_layout<'b>(layout: Layout<'b>) -> Layout<'b> {
        layout
            .children()
            .next()
            .expect("token frame has one child")
            .with_virtual_offset(layout.virtual_offset())
    }

    fn input_focused(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> bool {
        let mut probe = FocusProbe {
            target: &self.input_id,
            focused: false,
        };
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            Self::content_layout(layout),
            renderer,
            &mut probe,
        );
        probe.focused
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for TokenFrame<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<FrameState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(FrameState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(node.size(), vec![node])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::content_layout(layout),
                renderer,
                operation,
            );
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Backspace),
            ..
        }) = event
            && let Some(message) = self.on_remove_last.clone()
            && self.input_focused(tree, layout, renderer)
        {
            shell.publish(message);
            shell.capture_event();
            return;
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            Self::content_layout(layout),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        // A click on the frame's empty space focuses the text field.
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && !shell.is_event_captured()
            && cursor.is_over(layout.bounds())
        {
            let mut focus = operation::focusable::focus::<()>(self.input_id.clone());
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::content_layout(layout),
                renderer,
                &mut focus,
            );
            shell.capture_event();
        }

        if matches!(
            event,
            Event::Mouse(_) | Event::Keyboard(_) | Event::Touch(_)
        ) {
            let focused = self.input_focused(tree, layout, renderer);
            let state = tree.state.downcast_mut::<FrameState>();
            if state.focused != focused {
                state.focused = focused;
                shell.request_redraw();
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let interaction = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            Self::content_layout(layout),
            cursor,
            viewport,
            renderer,
        );
        if interaction == mouse::Interaction::None && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            interaction
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let focused = tree.state.downcast_ref::<FrameState>().focused;
        let cosmic = theme.cosmic();
        let container = theme.current_container();
        let background = Color::from(container.small_widget).scale_alpha(0.25);
        let border_color: Color = if focused {
            cosmic.accent.base.into()
        } else {
            container.component.divider.into()
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds: layout.bounds(),
                border: Border {
                    radius: cosmic.corner_radii.radius_xl.into(),
                    width: 2.0,
                    color: border_color,
                },
                ..Default::default()
            },
            Background::Color(background),
        );
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            Self::content_layout(layout),
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::content_layout(layout),
            renderer,
            viewport,
            translation,
        )
    }
}

/// Lays tokens out like words (wrapping onto new lines) followed by the text
/// field, which takes the rest of the last line or a line of its own when
/// less than `min_input_width` is left.
///
/// The text field is always child 0 of the widget tree, whatever the number
/// of tokens: iced reconciles widget state by position, so a field that moved
/// would lose its state (and focus) every time a token is added or removed.
struct TokenFlow<'a, Message> {
    input: Element<'a, Message>,
    tokens: Vec<Element<'a, Message>>,
    spacing: f32,
    min_input_width: f32,
}

impl<'a, Message> TokenFlow<'a, Message> {
    fn children_mut(&mut self) -> impl Iterator<Item = &mut Element<'a, Message>> {
        std::iter::once(&mut self.input).chain(self.tokens.iter_mut())
    }

    fn children_ref(&self) -> impl Iterator<Item = &Element<'a, Message>> {
        std::iter::once(&self.input).chain(self.tokens.iter())
    }
}

impl<Message> Widget<Message, Theme, Renderer> for TokenFlow<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.children_ref().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        let mut children: Vec<&mut Element<'_, Message>> = self.children_mut().collect();
        tree.diff_children(children.as_mut_slice());
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let max_width = limits.max().width;
        let loose = layout::Limits::new(Size::ZERO, Size::new(max_width, f32::INFINITY));
        let (input_tree, token_trees) = tree.children.split_first_mut().expect("input tree");

        let mut flow = Flow::new(max_width, self.spacing);
        for (token, token_tree) in self.tokens.iter_mut().zip(token_trees) {
            flow.push(token.as_widget_mut().layout(token_tree, renderer, &loose));
        }
        let remaining = max_width - flow.x;
        let input_width = if remaining >= self.min_input_width {
            remaining
        } else {
            max_width
        };
        let input_limits = layout::Limits::new(Size::ZERO, Size::new(input_width, f32::INFINITY));
        flow.push(
            self.input
                .as_widget_mut()
                .layout(input_tree, renderer, &input_limits),
        );

        let (size, mut nodes) = flow.finish();
        // Layout children follow the widget tree order: input first.
        nodes.rotate_right(1);
        layout::Node::with_children(size, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, state), child_layout) in self
                .children_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child.as_widget_mut().operate(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        for ((child, state), child_layout) in self
            .children_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state,
                event,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children_ref()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), child_layout)| {
                child.as_widget().mouse_interaction(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        for ((child, state), child_layout) in self
            .children_ref()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        // The input's overlay (e.g. its context menu) is the only one needed.
        self.input.as_widget_mut().overlay(
            &mut tree.children[0],
            layout
                .children()
                .next()?
                .with_virtual_offset(layout.virtual_offset()),
            renderer,
            viewport,
            translation,
        )
    }
}

/// Greedy line-wrapping placement; each line's items are vertically centered.
struct Flow {
    max_width: f32,
    spacing: f32,
    x: f32,
    nodes: Vec<layout::Node>,
    /// Index of the first node of every line.
    line_starts: Vec<usize>,
}

impl Flow {
    fn new(max_width: f32, spacing: f32) -> Self {
        Self {
            max_width,
            spacing,
            x: 0.0,
            nodes: Vec::new(),
            line_starts: vec![0],
        }
    }

    fn push(&mut self, node: layout::Node) {
        let width = node.size().width;
        if self.x > 0.0 && self.x + width > self.max_width {
            self.line_starts.push(self.nodes.len());
            self.x = 0.0;
        }
        self.nodes.push(node.move_to((self.x, 0.0)));
        self.x += width + self.spacing;
    }

    fn finish(mut self) -> (Size, Vec<layout::Node>) {
        let mut y = 0.0f32;
        let mut ends = self.line_starts.iter().skip(1).copied().collect::<Vec<_>>();
        ends.push(self.nodes.len());
        for (&start, end) in self.line_starts.iter().zip(ends) {
            let line = &mut self.nodes[start..end];
            let height = line.iter().map(|n| n.size().height).fold(0.0, f32::max);
            for node in line {
                let x = node.bounds().x;
                let dy = (height - node.size().height) / 2.0;
                node.move_to_mut((x, y + dy));
            }
            y += height + self.spacing;
        }
        let height = (y - self.spacing).max(0.0);
        (Size::new(self.max_width, height), self.nodes)
    }
}

/// Reports whether the focusable widget with `target` id is focused.
struct FocusProbe<'a> {
    target: &'a Id,
    focused: bool,
}

impl Operation for FocusProbe<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if id == Some(self.target) && state.is_focused() {
            self.focused = true;
        }
    }
}
