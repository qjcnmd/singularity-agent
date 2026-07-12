def parse_page(value):
    page = int(value)
    if page < 1:
        raise ValueError("page must be positive")
    return page
