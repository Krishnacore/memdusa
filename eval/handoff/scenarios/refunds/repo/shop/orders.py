from dataclasses import dataclass, field


@dataclass(frozen=True)
class Item:
    sku: str
    price_cents: int
    quantity: int = 1

    @property
    def total_cents(self) -> int:
        return self.price_cents * self.quantity


@dataclass
class Order:
    id: str
    items: list[Item] = field(default_factory=list)

    @property
    def total_cents(self) -> int:
        return sum(i.total_cents for i in self.items)


_ORDERS: dict[str, Order] = {}


def create_order(order_id: str, items: list[Item]) -> Order:
    order = Order(order_id, list(items))
    _ORDERS[order_id] = order
    return order


def get_order(order_id: str) -> Order:
    try:
        return _ORDERS[order_id]
    except KeyError:
        raise KeyError(f"unknown order {order_id}") from None


def reset() -> None:
    _ORDERS.clear()
