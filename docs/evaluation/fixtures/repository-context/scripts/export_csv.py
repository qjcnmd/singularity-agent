import csv


def write_rows(path, rows):
    with open(path, "w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=("reference", "amount"))
        writer.writeheader()
        writer.writerows(rows)
