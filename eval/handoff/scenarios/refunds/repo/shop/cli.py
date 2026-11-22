import argparse

from shop import orders


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="shop")
    sub = parser.add_subparsers(dest="cmd", required=True)
    show = sub.add_parser("total", help="print an order's total in cents")
    show.add_argument("order_id")
    args = parser.parse_args(argv)
    if args.cmd == "total":
        print(orders.get_order(args.order_id).total_cents)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
