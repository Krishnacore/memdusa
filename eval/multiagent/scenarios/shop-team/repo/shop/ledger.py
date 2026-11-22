"""Append-only ledger of money movements."""
from dataclasses import dataclass


@dataclass(frozen=True)
class Entry:
    kind: str
    order_id: int
    amount_cents: int
    note: str = ""


_ENTRIES: list[Entry] = []


def record(kind: str, order_id: int, amount_cents: int, note: str = "") -> Entry:
    entry = Entry(kind, order_id, amount_cents, note)
    _ENTRIES.append(entry)
    return entry


def entries(order_id: int | None = None) -> list[Entry]:
    return [e for e in _ENTRIES if order_id is None or e.order_id == order_id]


def reset() -> None:
    _ENTRIES.clear()
