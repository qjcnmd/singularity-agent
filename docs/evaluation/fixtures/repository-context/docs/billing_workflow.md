# Billing workflow

Billing entries are signed amounts: sales add value and refunds subtract value.
The billing report first aggregates every entry, then rounds the service fee to
cents, and finally adds that fee to the net amount for `grand_total`.
