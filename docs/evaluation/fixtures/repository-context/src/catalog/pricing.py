from decimal import Decimal


def annual_price(monthly_price):
    return Decimal(str(monthly_price)) * 12
