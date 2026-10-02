"""The theme editor: make themes of your own, with the whole screen as the
live preview."""

from __future__ import annotations

from typing import TYPE_CHECKING

from rich.text import Text
from textual import on
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import Screen
from textual.theme import Theme
from textual.widgets import (
    Button,
    Checkbox,
    Footer,
    Header,
    Input,
    Label,
    OptionList,
    Static,
)
from textual.widgets.option_list import Option

from .groups import ConfirmScreen
from .themes import (
    COLORS,
    DEFAULT_THEME,
    PREVIEW,
    ThemeError,
    colors_of,
    make_theme,
    reserved_names,
)

if TYPE_CHECKING:
    from .app import RmmApp


class ThemeEditorScreen(Screen):
    """Pick a theme to start from, change its colours and save it under a
    name of your own. The app wears the colours in the form as they are
    typed; closing puts back the theme last saved here, or else the one in
    use before."""

    app: RmmApp

    BINDINGS = [
        Binding("escape", "close", "Back"),
        Binding("ctrl+s", "save", "Save", priority=True),
    ]

    def __init__(self) -> None:
        super().__init__()
        #: The theme in use when the editor opened.
        self.original = DEFAULT_THEME
        #: The theme last saved here, if any.
        self.saved: str | None = None
        #: The theme the form was filled from.
        self.base: Theme | None = None

    def compose(self) -> ComposeResult:
        yield Header()
        with Horizontal(id="te-body"):
            with Vertical(id="te-list-pane"):
                yield Static("[b]Start from[/b]", id="te-list-title")
                yield OptionList(id="te-themes")
            with VerticalScroll(id="te-main"):
                with Horizontal(classes="te-row"):
                    yield Label("Name", classes="te-label")
                    yield Input(id="te-name", compact=True, max_length=32)
                for color in COLORS:
                    with Horizontal(classes="te-row"):
                        yield Label(color.capitalize(), classes="te-label")
                        yield Input(
                            id=f"te-{color}",
                            classes="te-color",
                            compact=True,
                            restrict=r"#?[0-9a-fA-F]*",
                            max_length=7,
                        )
                        yield Static("", id=f"te-swatch-{color}", classes="te-swatch")
                yield Checkbox("Dark theme", id="te-dark", compact=True)
                with Horizontal(id="te-buttons"):
                    yield Button("Save", variant="success", id="te-save")
                    yield Button("Delete", variant="error", id="te-delete")
                    yield Button("Close", id="te-close")
                yield Static("", id="te-status")
                with Vertical(id="te-preview"):
                    yield Static("[b]Preview[/b]")
                    yield Static(
                        "[$primary]Primary[/]  [$secondary]Secondary[/]  [$accent]Accent[/]  "
                        "[$success]Success[/]  [$warning]Warning[/]  [$error]Error[/]"
                    )
                    yield Static("Ordinary text, and [$text-muted]muted text[/].")
                    yield Static("A panel, like the menu bar.", id="te-preview-panel")
                    with Horizontal(id="te-preview-buttons"):
                        for variant in ("default", "primary", "success", "warning", "error"):
                            button = Button(variant.capitalize(), variant=variant)
                            button.can_focus = False
                            yield button
        yield Footer()

    def on_mount(self) -> None:
        self.title = "Theme editor"
        if self.app.theme != PREVIEW:
            self.original = self.app.theme
        start = self.original if self.original in self.choices() else DEFAULT_THEME
        self.fill(start)
        self.load(start)
        self.query_one("#te-themes").focus()

    # --- theme list -----------------------------------------------------------

    def choices(self) -> list[str]:
        """The themes to start from: the user's own, then the rest."""
        own = sorted(self.app.state.custom_themes)
        rest = [
            name
            for name, theme in self.app.available_themes.items()
            if name not in own and name != PREVIEW and colors_of(theme) is not None
        ]
        return own + rest

    def fill(self, highlight: str) -> None:
        own = self.app.state.custom_themes
        names = self.choices()
        themes = self.query_one("#te-themes", OptionList)
        with themes.prevent(OptionList.OptionHighlighted):
            themes.clear_options()
            themes.add_options(
                [
                    Option(Text.assemble(name, ("  yours", "dim") if name in own else ""), id=name)
                    for name in names
                ]
            )
            themes.highlighted = names.index(highlight)

    @on(OptionList.OptionHighlighted, "#te-themes")
    def theme_highlighted(self, event: OptionList.OptionHighlighted) -> None:
        self.load(str(event.option.id))

    @on(OptionList.OptionSelected, "#te-themes")
    def theme_selected(self) -> None:
        self.query_one("#te-name", Input).focus()

    def new_name(self, base: str) -> str:
        """A free name for a theme made from the built-in ``base``."""
        taken = reserved_names() | set(self.app.state.custom_themes)
        name, n = f"{base}-custom"[:32], 2
        while name in taken:
            name, n = f"{base[:24]}-custom-{n}", n + 1
        return name

    def load(self, name: str) -> None:
        """Fill the form from theme ``name`` and show it."""
        theme = self.app.available_themes[name]
        own = name in self.app.state.custom_themes
        self.base = theme
        colors = colors_of(theme) or {}
        with self.prevent(Input.Changed, Checkbox.Changed):
            self.query_one("#te-name", Input).value = name if own else self.new_name(name)
            for color in COLORS:
                self.query_one(f"#te-{color}", Input).value = colors.get(color, "")
            self.query_one("#te-dark", Checkbox).value = theme.dark
        self.query_one("#te-delete", Button).disabled = not own
        self.set_status("")
        self.preview()

    # --- form -------------------------------------------------------------------

    def set_status(self, text: str) -> None:
        self.query_one("#te-status", Static).update(text)

    def build(self, name: str) -> Theme:
        """The theme in the form. Raises :class:`ThemeError` if it is not one."""
        colors = {color: self.query_one(f"#te-{color}", Input).value for color in COLORS}
        colors = {k: v if v.startswith("#") else f"#{v}" for k, v in colors.items()}
        return make_theme(name, colors, self.query_one("#te-dark", Checkbox).value, self.base)

    @on(Input.Changed, ".te-color")
    @on(Checkbox.Changed, "#te-dark")
    def changed(self) -> None:
        self.set_status("")
        self.preview()

    def preview(self) -> None:
        """Show the form's colours on the app, once they are all valid."""
        try:
            theme = self.build(PREVIEW)
        except ThemeError as e:
            self.set_status(f"[$error]{e}[/]")
            return
        for color in COLORS:
            self.query_one(f"#te-swatch-{color}", Static).styles.background = getattr(theme, color)
        self.app.register_theme(theme)
        if self.app.theme == PREVIEW:
            # Same name, new colours: have the app apply it again.
            self.app.mutate_reactive(App.theme)
        else:
            self.app.theme = PREVIEW

    @on(Button.Pressed, "#te-save")
    @on(Input.Submitted, "#te-name")
    def action_save(self) -> None:
        name = self.query_one("#te-name", Input).value.strip().lower()
        if name in reserved_names():
            self.set_status(f"[$error]{name} is a built-in theme: give yours another name.[/]")
            return
        try:
            theme = self.build(name)
        except ThemeError as e:
            self.set_status(f"[$error]{e}[/]")
            return
        state = self.app.state
        replaced = name in state.custom_themes
        state.custom_themes[name] = theme
        # Also the theme to start with, should the app be quit from here.
        state.theme = name
        state.save()
        self.app.register_theme(theme)
        self.saved = name
        self.base = theme
        self.fill(name)
        self.query_one("#te-name", Input).value = name
        self.query_one("#te-delete", Button).disabled = False
        self.set_status(f"{'Updated' if replaced else 'Saved'} {name}; it is now your theme.")

    @on(Button.Pressed, "#te-delete")
    def delete(self) -> None:
        themes = self.query_one("#te-themes", OptionList)
        if themes.highlighted is None:
            return
        name = str(themes.get_option_at_index(themes.highlighted).id)
        if name not in self.app.state.custom_themes:
            return

        def confirmed(yes: bool | None) -> None:
            if not yes:
                return
            state = self.app.state
            del state.custom_themes[name]
            if state.theme == name:
                state.theme = None
            state.save()
            self.app.unregister_theme(name)
            if self.saved == name:
                self.saved = None
            if self.original == name:
                self.original = DEFAULT_THEME
            self.fill(DEFAULT_THEME)
            self.load(DEFAULT_THEME)
            self.set_status(f"Deleted {name}.")

        self.app.push_screen(ConfirmScreen(f"Delete the theme {name}?"), confirmed)

    @on(Button.Pressed, "#te-close")
    def action_close(self) -> None:
        final = self.saved or self.original
        self.app.theme = final if final in self.app.available_themes else DEFAULT_THEME
        self.app.unregister_theme(PREVIEW)
        self.app.pop_screen()
