// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/pricing.cpp @ rust-port-baseline

//! The prices, discounts, taxes and refunds of a sale or return line.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::decimal::{
    Decimal, OP_DIV, OP_MINUS, OP_MULT, OP_PLUS, decimal_t_op, decimal_t_op_in_place, itodec, strtodec,
};
use super::r_params::atoi;

/// `ds_pricing_t`
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Pricing {
    pub(crate) wholesale_cost: Decimal,
    pub(crate) list_price: Decimal,
    pub(crate) sales_price: Decimal,
    pub(crate) quantity: i32,
    pub(crate) ext_discount_amt: Decimal,
    pub(crate) ext_sales_price: Decimal,
    pub(crate) ext_wholesale_cost: Decimal,
    pub(crate) ext_list_price: Decimal,
    pub(crate) tax_pct: Decimal,
    pub(crate) ext_tax: Decimal,
    pub(crate) coupon_amt: Decimal,
    pub(crate) ship_cost: Decimal,
    pub(crate) ext_ship_cost: Decimal,
    pub(crate) net_paid: Decimal,
    pub(crate) net_paid_inc_tax: Decimal,
    pub(crate) net_paid_inc_ship: Decimal,
    pub(crate) net_paid_inc_ship_tax: Decimal,
    pub(crate) net_profit: Decimal,
    pub(crate) refunded_cash: Decimal,
    pub(crate) reversed_charge: Decimal,
    pub(crate) store_credit: Decimal,
    pub(crate) fee: Decimal,
    pub(crate) net_loss: Decimal,
}

/// `ds_limits_t`: `(nId, szQuantity, szMarkUp, szDiscount, szWholesale, szCoupon)`
type Limits = (
    i32,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

const A_PRICE_LIMITS: [Limits; 12] = [
    (
        CS_PRICING,
        CS_QUANTITY_MAX,
        CS_MARKUP_MAX,
        CS_DISCOUNT_MAX,
        CS_WHOLESALE_MAX,
        CS_COUPON_MAX,
    ),
    (
        SS_PRICING,
        SS_QUANTITY_MAX,
        SS_MARKUP_MAX,
        SS_DISCOUNT_MAX,
        SS_WHOLESALE_MAX,
        SS_COUPON_MAX,
    ),
    (
        WS_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        CR_PRICING,
        CS_QUANTITY_MAX,
        CS_MARKUP_MAX,
        CS_DISCOUNT_MAX,
        CS_WHOLESALE_MAX,
        CS_COUPON_MAX,
    ),
    (
        SR_PRICING,
        SS_QUANTITY_MAX,
        SS_MARKUP_MAX,
        SS_DISCOUNT_MAX,
        SS_WHOLESALE_MAX,
        SS_COUPON_MAX,
    ),
    (
        WR_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        S_PLINE_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        S_CLIN_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        S_WLIN_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        S_SRET_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        S_CRET_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
    (
        S_WRET_PRICING,
        WS_QUANTITY_MAX,
        WS_MARKUP_MAX,
        WS_DISCOUNT_MAX,
        WS_WHOLESALE_MAX,
        WS_COUPON_MAX,
    ),
];

/// The constants of `set_pricing`.
#[derive(Default)]
pub(crate) struct State {
    d_markup_min: Decimal,
    d_discount_min: Decimal,
    d_wholesale_min: Decimal,
    d_zero: Decimal,
    d_one_half: Decimal,
    d9pct: Decimal,
    d_one: Decimal,
    d_hundred: Decimal,
}

impl Ctx {
    /// `set_pricing`: the sale prices of a line (`*_PRICING` of a sales table), or the refunds of a
    /// return (of a returns table) from the prices of its sale.
    pub(crate) fn set_pricing(&mut self, n_tab_id: i32, p: &mut Pricing) {
        if !self.init.set_pricing_init {
            let s = &mut self.pricing;
            s.d_markup_min = strtodec(b"0.00");
            s.d_discount_min = strtodec(b"0.00");
            s.d_wholesale_min = strtodec(b"1.00");
            s.d_zero = strtodec(b"0.00");
            s.d_one_half = strtodec(b"0.50");
            s.d9pct = strtodec(b"0.09");
            s.d_hundred = strtodec(b"100.00");
            s.d_one = strtodec(b"1.00");
            self.init.set_pricing_init = true;
        }
        let limits = A_PRICE_LIMITS
            .iter()
            .rev()
            .find(|l| l.0 == n_tab_id)
            .unwrap_or_else(|| panic!("dsdgen: No pricing limits defined"));
        let n_quantity_max = atoi(limits.1.as_bytes());
        let d_discount_max = strtodec(limits.3.as_bytes());
        let d_markup_max = strtodec(limits.2.as_bytes());
        let d_wholesale_max = strtodec(limits.4.as_bytes());
        let (d_markup_min, d_discount_min, d_wholesale_min) = (
            self.pricing.d_markup_min,
            self.pricing.d_discount_min,
            self.pricing.d_wholesale_min,
        );
        let (d_zero, d_one_half, d9pct, d_one, d_hundred) = (
            self.pricing.d_zero,
            self.pricing.d_one_half,
            self.pricing.d9pct,
            self.pricing.d_one,
            self.pricing.d_hundred,
        );
        let n_quantity_min = 1;
        match n_tab_id {
            SS_PRICING | CS_PRICING | WS_PRICING | S_PLINE_PRICING | S_CLIN_PRICING | S_WLIN_PRICING => {
                p.quantity = self.genrand_integer(DIST_UNIFORM, n_quantity_min, n_quantity_max, 0, n_tab_id);
                let d_quantity = itodec(p.quantity);
                self.genrand_decimal(
                    &mut p.wholesale_cost,
                    DIST_UNIFORM,
                    &d_wholesale_min,
                    &d_wholesale_max,
                    None,
                    n_tab_id,
                );
                decimal_t_op(&mut p.ext_wholesale_cost, OP_MULT, &d_quantity, &p.wholesale_cost);

                let mut d_markup = Decimal::default();
                self.genrand_decimal(
                    &mut d_markup,
                    DIST_UNIFORM,
                    &d_markup_min,
                    &d_markup_max,
                    None,
                    n_tab_id,
                );
                decimal_t_op_in_place(&mut d_markup, OP_PLUS, &d_one);
                decimal_t_op(&mut p.list_price, OP_MULT, &p.wholesale_cost, &d_markup);

                let mut d_discount = Decimal::default();
                self.genrand_decimal(
                    &mut d_discount,
                    DIST_UNIFORM,
                    &d_discount_min,
                    &d_discount_max,
                    None,
                    n_tab_id,
                );
                d_discount.number *= -1;
                decimal_t_op(&mut p.ext_discount_amt, OP_PLUS, &d_discount, &d_one);
                decimal_t_op(&mut p.sales_price, OP_MULT, &p.list_price, &p.ext_discount_amt);

                decimal_t_op(&mut p.ext_list_price, OP_MULT, &p.list_price, &d_quantity);
                decimal_t_op(&mut p.ext_sales_price, OP_MULT, &p.sales_price, &d_quantity);
                decimal_t_op(&mut p.ext_discount_amt, OP_MINUS, &p.ext_list_price, &p.ext_sales_price);

                let mut d_coupon = Decimal::default();
                self.genrand_decimal(&mut d_coupon, DIST_UNIFORM, &d_zero, &d_one, None, n_tab_id);
                let n_coupon_usage = self.genrand_integer(DIST_UNIFORM, 1, 100, 0, n_tab_id);
                if n_coupon_usage <= 20 {
                    decimal_t_op(&mut p.coupon_amt, OP_MULT, &p.ext_sales_price, &d_coupon);
                } else {
                    p.coupon_amt = d_zero;
                }
                decimal_t_op(&mut p.net_paid, OP_MINUS, &p.ext_sales_price, &p.coupon_amt);

                let mut d_shipping = Decimal::default();
                self.genrand_decimal(&mut d_shipping, DIST_UNIFORM, &d_zero, &d_one_half, None, n_tab_id);
                decimal_t_op(&mut p.ship_cost, OP_MULT, &p.list_price, &d_shipping);
                decimal_t_op(&mut p.ext_ship_cost, OP_MULT, &p.ship_cost, &d_quantity);
                decimal_t_op(&mut p.net_paid_inc_ship, OP_PLUS, &p.net_paid, &p.ext_ship_cost);
                self.genrand_decimal(&mut p.tax_pct, DIST_UNIFORM, &d_zero, &d9pct, None, n_tab_id);
                decimal_t_op(&mut p.ext_tax, OP_MULT, &p.net_paid, &p.tax_pct);
                decimal_t_op(&mut p.net_paid_inc_tax, OP_PLUS, &p.net_paid, &p.ext_tax);
                decimal_t_op(&mut p.net_paid_inc_ship_tax, OP_PLUS, &p.net_paid_inc_ship, &p.ext_tax);
                decimal_t_op(&mut p.net_profit, OP_MINUS, &p.net_paid, &p.ext_wholesale_cost);
            }
            CR_PRICING | SR_PRICING | WR_PRICING => {
                let d_quantity = itodec(p.quantity);
                decimal_t_op(&mut p.ext_wholesale_cost, OP_MULT, &d_quantity, &p.wholesale_cost);
                decimal_t_op(&mut p.ext_list_price, OP_MULT, &p.list_price, &d_quantity);
                decimal_t_op(&mut p.ext_sales_price, OP_MULT, &p.sales_price, &d_quantity);
                p.net_paid = p.ext_sales_price;
                let mut d_shipping = Decimal::default();
                self.genrand_decimal(&mut d_shipping, DIST_UNIFORM, &d_zero, &d_one_half, None, n_tab_id);
                decimal_t_op(&mut p.ship_cost, OP_MULT, &p.list_price, &d_shipping);
                decimal_t_op(&mut p.ext_ship_cost, OP_MULT, &p.ship_cost, &d_quantity);
                decimal_t_op(&mut p.net_paid_inc_ship, OP_PLUS, &p.net_paid, &p.ext_ship_cost);
                decimal_t_op(&mut p.ext_tax, OP_MULT, &p.net_paid, &p.tax_pct);
                decimal_t_op(&mut p.net_paid_inc_tax, OP_PLUS, &p.net_paid, &p.ext_tax);
                decimal_t_op(&mut p.net_paid_inc_ship_tax, OP_PLUS, &p.net_paid_inc_ship, &p.ext_tax);
                decimal_t_op(&mut p.net_profit, OP_MINUS, &p.net_paid, &p.ext_wholesale_cost);

                let n_cash_pct = self.genrand_integer(DIST_UNIFORM, 0, 100, 0, n_tab_id);
                let mut d_temp = itodec(n_cash_pct);
                decimal_t_op(&mut p.refunded_cash, OP_DIV, &d_temp, &d_hundred);
                let net_paid = p.net_paid;
                decimal_t_op_in_place(&mut p.refunded_cash, OP_MULT, &net_paid);

                let n_credit_pct = self.genrand_integer(DIST_UNIFORM, 1, 100, 0, n_tab_id);
                let mut d_temp2 = itodec(n_credit_pct);
                decimal_t_op(&mut d_temp, OP_DIV, &d_temp2, &d_hundred);
                decimal_t_op(&mut d_temp2, OP_MINUS, &p.net_paid, &p.refunded_cash);
                decimal_t_op(&mut p.reversed_charge, OP_MULT, &d_temp2, &d_temp);
                decimal_t_op(&mut p.store_credit, OP_MINUS, &p.net_paid, &p.reversed_charge);
                let refunded_cash = p.refunded_cash;
                decimal_t_op_in_place(&mut p.store_credit, OP_MINUS, &refunded_cash);
                self.genrand_decimal(
                    &mut p.fee,
                    DIST_UNIFORM,
                    &d_one_half,
                    &d_hundred,
                    Some(&d_zero),
                    n_tab_id,
                );
                decimal_t_op(&mut p.net_loss, OP_MINUS, &p.net_paid_inc_ship_tax, &p.store_credit);
                let (reversed_charge, fee) = (p.reversed_charge, p.fee);
                decimal_t_op_in_place(&mut p.net_loss, OP_MINUS, &refunded_cash);
                decimal_t_op_in_place(&mut p.net_loss, OP_MINUS, &reversed_charge);
                decimal_t_op_in_place(&mut p.net_loss, OP_PLUS, &fee);
            }
            _ => {}
        }
    }
}
