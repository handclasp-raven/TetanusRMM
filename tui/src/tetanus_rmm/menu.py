"""A menu bar: drop-down menus over a screen's key bindings."""

from __future__ import annotations

from rich.text import Text
from textual import events, on
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal
from textual.screen import ModalScreen
from textual.widgets import OptionList, Static
from textual.widgets.option_list import Option


class MenuTitle(Static):
    """One menu's name in the bar; a click opens it."""

    def __init__(self, label: str, index: int) -> None:
        super().__init__(f" {label} ", classes="menu-title")
        self.index = index

    def on_click(self, event: events.Click) -> None:
        event.stop()
        self.query_ancestor(MenuBar).open(self.index)


class MenuBar(Horizontal):
    """Groups the screen's bindings into menus. ``menus`` maps each menu's
    name to the actions in it; an item takes its label and key from the
    screen's binding for that action, and is greyed out where the screen's
    ``check_action`` refuses it."""

    def __init__(self, menus: dict[str, list[str]], id: str | None = None) -> None:  # noqa: A002
        super().__init__(id=id)
        self.menus = menus

    def compose(self) -> ComposeResult:
        for index, label in enumerate(self.menus):
            yield MenuTitle(label, index)

    @property
    def titles(self) -> list[MenuTitle]:
        return list(self.query(MenuTitle))

    def options(self, index: int) -> list[Option]:
        """The items of menu ``index``, as they stand now."""
        screen = self.screen
        bindings = {b.action: b for b in screen.BINDINGS if isinstance(b, Binding)}
        items = [bindings[action] for action in list(self.menus.values())[index]]
        width = max(len(b.description) for b in items)
        return [
            Option(
                Text.assemble(f"{b.description:<{width}}  ", (self.app.get_key_display(b), "dim")),
                id=b.action,
                # Another namespace's actions (app.quit) are not the screen's to refuse.
                disabled="." not in b.action and not screen.check_action(b.action, ()),
            )
            for b in items
        ]

    def open(self, index: int = 0) -> None:
        screen = self.screen

        def chosen(action: str | None) -> None:
            if action is not None:
                screen.run_worker(screen.run_action(action), group="menu")

        self.app.push_screen(MenuScreen(self, index), chosen)


class MenuScreen(ModalScreen[str | None]):
    """An open menu, drawn under its title. Dismissed with the chosen
    action, or ``None`` if closed."""

    BINDINGS = [
        Binding("escape,f10", "close", "Close"),
        Binding("left", "switch(-1)", "Previous menu", show=False),
        Binding("right", "switch(1)", "Next menu", show=False),
    ]

    def __init__(self, bar: MenuBar, index: int) -> None:
        super().__init__()
        self.bar = bar
        self.index = index

    def compose(self) -> ComposeResult:
        yield OptionList(id="menu-list")

    def on_mount(self) -> None:
        self.show(self.index)

    def show(self, index: int) -> None:
        titles = self.bar.titles
        titles[self.index].remove_class("-open")
        self.index = index
        titles[index].add_class("-open")
        region = titles[index].region
        options = self.query_one("#menu-list", OptionList)
        options.clear_options()
        options.add_options(self.bar.options(index))
        options.styles.offset = (region.x, region.bottom)
        options.action_first()
        options.focus()

    def on_unmount(self) -> None:
        self.bar.titles[self.index].remove_class("-open")

    def action_switch(self, step: int) -> None:
        self.show((self.index + step) % len(self.bar.titles))

    def action_close(self) -> None:
        self.dismiss(None)

    @on(OptionList.OptionSelected, "#menu-list")
    def chosen(self, event: OptionList.OptionSelected) -> None:
        self.dismiss(event.option.id)

    def on_click(self, event: events.Click) -> None:
        if event.widget is not self:
            return
        # Outside the menu: another title opens that menu, anywhere else closes.
        for title in self.bar.titles:
            if title.region.contains(event.screen_x, event.screen_y):
                self.show(title.index)
                return
        self.dismiss(None)
