"""Colour themes of our own, offered next to Textual's built-in ones, and
the ones the user makes in the theme editor."""

from __future__ import annotations

import re

from textual.color import Color, ColorParseError
from textual.theme import BUILTIN_THEMES, Theme

#: The colours of a theme the editor offers, in its order.
COLORS = (
    "primary",
    "secondary",
    "accent",
    "foreground",
    "background",
    "surface",
    "panel",
    "success",
    "warning",
    "error",
)

#: The theme the editor shows its unsaved changes with. Never saved.
PREVIEW = "preview"

_NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,31}")
_HEX = re.compile(r"#[0-9a-fA-F]{6}")


class ThemeError(ValueError):
    """A theme the user typed that cannot be used."""


#: The theme a new installation starts with.
DEFAULT_THEME = "tetanus"

THEMES = [
    # The default: bright orange, purple and pink on very dark purple.
    Theme(
        name="tetanus",
        primary="#FF9000",
        secondary="#CF8CFF",
        accent="#FF5FD2",
        warning="#FFE000",
        error="#FF4D6A",
        success="#4DFFA0",
        foreground="#FFFFFF",
        background="#020003",
        surface="#050109",
        panel="#0B0314",
        text_alpha=1.0,
    ),
    Theme(
        name="everforest",
        primary="#A7C080",
        secondary="#7FBBB3",
        accent="#DBBC7F",
        warning="#E69875",
        error="#E67E80",
        success="#A7C080",
        foreground="#D3C6AA",
        background="#272E33",
        surface="#2E383C",
        panel="#374145",
    ),
    Theme(
        name="kanagawa",
        primary="#7E9CD8",
        secondary="#957FB8",
        accent="#FFA066",
        warning="#E6C384",
        error="#E82424",
        success="#98BB6C",
        foreground="#DCD7BA",
        background="#1F1F28",
        surface="#2A2A37",
        panel="#363646",
    ),
    Theme(
        name="ayu-dark",
        primary="#39BAE6",
        secondary="#D2A6FF",
        accent="#FFB454",
        warning="#FFB454",
        error="#F26D78",
        success="#AAD94C",
        foreground="#BFBDB6",
        background="#0B0E14",
        surface="#11151C",
        panel="#1B2029",
    ),
    Theme(
        name="github-dark",
        primary="#58A6FF",
        secondary="#BC8CFF",
        accent="#F78166",
        warning="#D29922",
        error="#F85149",
        success="#3FB950",
        foreground="#C9D1D9",
        background="#0D1117",
        surface="#161B22",
        panel="#21262D",
    ),
    Theme(
        name="github-light",
        primary="#0969DA",
        secondary="#8250DF",
        accent="#BC4C00",
        warning="#9A6700",
        error="#CF222E",
        success="#1A7F37",
        foreground="#1F2328",
        background="#FFFFFF",
        surface="#F6F8FA",
        panel="#EAEEF2",
        dark=False,
    ),
    # Old phosphor terminals.
    Theme(
        name="green-screen",
        primary="#33FF66",
        secondary="#1FA347",
        accent="#B6FF8A",
        warning="#D7FF5F",
        error="#FF5F5F",
        success="#33FF66",
        foreground="#7CFF9B",
        background="#020A04",
        surface="#06150A",
        panel="#0B2412",
    ),
    Theme(
        name="amber-screen",
        primary="#FFB000",
        secondary="#B37B00",
        accent="#FFD166",
        warning="#FFD166",
        error="#FF5F3C",
        success="#C9E265",
        foreground="#FFC857",
        background="#0C0700",
        surface="#171000",
        panel="#261A00",
    ),
    Theme(
        name="high-contrast",
        primary="#FFFF00",
        secondary="#00FFFF",
        accent="#FF00FF",
        warning="#FFA500",
        error="#FF4040",
        success="#00FF00",
        foreground="#FFFFFF",
        background="#000000",
        surface="#000000",
        panel="#1A1A1A",
    ),
]


def reserved_names() -> set[str]:
    """Names a theme of the user's own may not take."""
    return {*BUILTIN_THEMES, *(theme.name for theme in THEMES), PREVIEW}


def colors_of(theme: Theme) -> dict[str, str] | None:
    """Every colour in :data:`COLORS` as ``#RRGGBB``, those the theme leaves
    to be derived included. ``None`` for a theme of terminal colours, which
    have no fixed value."""
    if theme.ansi:
        return None
    generated = theme.to_color_system().generate()
    try:
        # The theme's own value where it has one: the generated ones are rounded.
        parsed = {key: Color.parse(getattr(theme, key) or generated[key]) for key in COLORS}
    except (ColorParseError, KeyError):
        return None
    return {key: f"#{c.r:02X}{c.g:02X}{c.b:02X}" for key, c in parsed.items()}


def make_theme(name: str, colors: dict[str, str], dark: bool, base: Theme | None = None) -> Theme:
    """A theme of the user's own. What the editor does not offer (how far
    shades spread, text opacity, extra variables) is taken from ``base``.
    Raises :class:`ThemeError` on a bad name or colour."""
    if not _NAME.fullmatch(name):
        raise ThemeError(
            "The name must be 1 to 32 lower-case letters, digits, - or _, "
            "starting with a letter or digit."
        )
    for key in COLORS:
        if not _HEX.fullmatch(colors.get(key, "")):
            raise ThemeError(f"{key.capitalize()} must be a colour like #FF9000.")
    extra = {}
    if base is not None:
        extra = {
            "luminosity_spread": base.luminosity_spread,
            "text_alpha": base.text_alpha,
            "variables": dict(base.variables),
        }
    return Theme(name=name, dark=dark, **{key: colors[key].upper() for key in COLORS}, **extra)


def to_json(theme: Theme) -> dict:
    return {
        **{key: getattr(theme, key) for key in COLORS},
        "dark": theme.dark,
        "luminosity_spread": theme.luminosity_spread,
        "text_alpha": theme.text_alpha,
        "variables": theme.variables,
    }


def from_json(raw: object) -> dict[str, Theme]:
    """The user's themes by name, from the state file; what is not a usable
    theme there is left out."""
    themes: dict[str, Theme] = {}
    if not isinstance(raw, dict):
        return themes
    for name, value in raw.items():
        if not isinstance(value, dict) or name in reserved_names():
            continue
        try:
            theme = make_theme(name, value, bool(value.get("dark", True)))
            theme.luminosity_spread = float(value.get("luminosity_spread", 0.15))
            theme.text_alpha = float(value.get("text_alpha", 0.95))
        except (ThemeError, TypeError, ValueError):
            continue
        variables = value.get("variables")
        if isinstance(variables, dict):
            theme.variables = {str(k): str(v) for k, v in variables.items()}
        themes[name] = theme
    return themes
