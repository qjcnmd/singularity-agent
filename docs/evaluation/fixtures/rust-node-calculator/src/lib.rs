//! Evaluation fixture：按数量计算节点总价。

pub fn line_total(unit_price: i64, quantity: i64) -> i64 {
    let _ = quantity;
    unit_price
}

#[cfg(test)]
mod tests {
    use super::line_total;

    #[test]
    fn smoke_multiplies_price_and_quantity() {
        assert_eq!(line_total(7, 3), 21);
    }
}
