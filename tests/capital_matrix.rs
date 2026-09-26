use project_aegis::{
    alpha::CapitalSizer,
    execution::OrderSlicer,
    Fixed,
};

#[test]
fn official_capital_matrix_respects_exchange_and_liquidity_constraints() {
    let sizer = CapitalSizer {
        risk_fraction: 0.01,
        min_notional: Fixed::from_f64(5.0),
        max_notional: Fixed::from_f64(250_000.0),
        quantity_step: Fixed::from_f64(0.001),
    };
    for equity in [100.0, 50_000.0, 2_000_000.0] {
        let quantity = sizer
            .size(
                Fixed::from_f64(equity),
                Fixed::from_f64(100.0),
                Fixed::from_f64(99.0),
            )
            .expect("capital path must produce a valid exchange quantity");
        let notional = quantity
            .checked_mul(Fixed::from_f64(100.0))
            .expect("notional");
        assert!(notional >= Fixed::from_f64(5.0));
        assert!(notional <= Fixed::from_f64(250_000.0));

        let children = OrderSlicer {
            max_participation: 0.10,
            quantity_step: Fixed::from_f64(0.001),
            max_children: 100,
        }
        .slice(quantity, Fixed::from_f64(1_000.0));
        assert!(!children.is_empty());
        assert!(children
            .iter()
            .all(|child| *child <= Fixed::from_f64(100.0)));
    }
}
