"""Loads tools/dashboards/thresholds.toml, optionally overlaid by a deployer's partial file. Standard library only."""

import re
import tomllib
from pathlib import Path

DEFAULT = Path(__file__).resolve().parent / "thresholds.toml"
_DURATION = re.compile(r"^[1-9][0-9]*[smhd]$")


class Refused(ValueError):
    """A thresholds file that is not accepted: the message says where."""


def _read(path):
    with open(path, "rb") as handle:
        return tomllib.load(handle)


def _number(where, value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise Refused(f"{where}: expected a number, got {value!r}")
    return value


def _duration(where, value):
    if not isinstance(value, str) or not _DURATION.match(value):
        raise Refused(f"{where}: expected a duration such as 5m, got {value!r}")
    return value


def load(override=None):
    """The default thresholds, with the signals and keys of `override` (a path) replaced. Refuses what it cannot read."""
    document = _read(DEFAULT)
    base = {
        "window": document["defaults"]["window"],
        "signals": {k: dict(v) for k, v in document["signals"].items()},
        "absence": dict(document["absence"]),
    }
    if override is not None:
        extra = _read(override)
        for key in extra:
            if key not in ("defaults", "signals", "absence"):
                raise Refused(f"unknown table {key!r}")
        for key, value in extra.get("defaults", {}).items():
            if key != "window":
                raise Refused(f"defaults.{key}: unknown key")
            base["window"] = _duration("defaults.window", value)
        for key, value in extra.get("absence", {}).items():
            if key not in ("window", "for"):
                raise Refused(f"absence.{key}: unknown key")
            base["absence"][key] = _duration(f"absence.{key}", value)
        for name, keys in extra.get("signals", {}).items():
            if name not in base["signals"]:
                raise Refused(f"signals.{name}: unknown signal")
            if not isinstance(keys, dict):
                raise Refused(f"signals.{name}: expected a table")
            for key, value in keys.items():
                where = f"signals.{name}.{key}"
                if key in ("warning", "critical"):
                    base["signals"][name][key] = _number(where, value)
                elif key == "for":
                    base["signals"][name][key] = _duration(where, value)
                else:
                    raise Refused(f"{where}: unknown key")
    _duration("defaults.window", base["window"])
    for key in ("window", "for"):
        _duration(f"absence.{key}", base["absence"][key])
    for name, keys in base["signals"].items():
        for key in ("warning", "critical"):
            _number(f"signals.{name}.{key}", keys[key])
        _duration(f"signals.{name}.for", keys["for"])
        if keys["warning"] >= keys["critical"]:
            raise Refused(f"signals.{name}: warning must be below critical")
    return base


def steps(signal):
    """The (value, colour) steps of a stat panel for `signal`, from the defaults."""
    keys = load()["signals"][signal]
    return [(None, "green"), (keys["warning"], "orange"), (keys["critical"], "red")]
