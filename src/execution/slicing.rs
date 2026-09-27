use crate::domain::Fixed;

/// Produces deterministic TWAP/VWAP child quantities for orders whose size
/// would exceed the configured visible-liquidity participation ceiling.
#[derive(Debug, Clone, Copy)]
pub struct OrderSlicer {
    pub max_participation: f64,
    pub quantity_step: Fixed,
    pub max_children: usize,
}

impl OrderSlicer {
    #[must_use]
    pub fn slice(self, total: Fixed, visible_quantity: Fixed) -> Vec<Fixed> {
        if total.0 <= 0 || visible_quantity.0 <= 0 || self.quantity_step.0 <= 0 {
            return Vec::new();
        }
        let per_child =
            Fixed::from_f64(visible_quantity.as_f64() * self.max_participation.clamp(0.001, 1.0))
                .floor_to(self.quantity_step)
                .max(self.quantity_step);
        let mut remaining = total.floor_to(self.quantity_step);
        let mut children = Vec::new();
        while remaining.0 > 0 && children.len() < self.max_children {
            let child = remaining.min(per_child).floor_to(self.quantity_step);
            if child.0 <= 0 {
                break;
            }
            children.push(child);
            remaining = remaining - child;
        }
        // Never silently exceed the configured child cap. The unscheduled tail
        // remains unexposed and can be reconsidered against a newer book.
        children
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_order_is_bounded_by_visible_participation() {
        let slicer = OrderSlicer {
            max_participation: 0.10,
            quantity_step: Fixed::from_f64(0.001),
            max_children: 100,
        };
        let children = slicer.slice(Fixed::from_f64(25.0), Fixed::from_f64(10.0));
        assert_eq!(children.len(), 25);
        assert!(children
            .iter()
            .all(|quantity| *quantity <= Fixed::from_f64(1.0)));
    }
}
