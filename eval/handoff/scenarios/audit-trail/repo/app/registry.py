"""A tiny in-memory name -> value registry (already implemented)."""

_VALUES: dict[str, int] = {}


def register(name: str, value: int) -> None:
    _VALUES[name] = value


def get(name: str) -> int:
    return _VALUES[name]


def all_names() -> list[str]:
    return list(_VALUES)


def reset() -> None:
    _VALUES.clear()
