// Ported from duckdb/extension/tpcds/dsdgen/include/dsdgen-c/constants.h @ rust-port-baseline

//! The generator's constants (`constants.h`), plus the few per-table ones from the `w_*.h`
//! headers, `dist.h`, `tpcds.idx.h` and `genrand.h`. Only the constants the port uses are here.

pub(crate) const DATA_START_DATE: &str = "1998-01-01";
pub(crate) const DATA_END_DATE: &str = "2003-12-31";

pub(crate) const C_PREFERRED_PCT: i32 = 50;
pub(crate) const CC_EMPLOYEE_MAX: i32 = 7;
pub(crate) const CP_CATALOGS_PER_YEAR: i32 = 18;
pub(crate) const CR_RETURN_PCT: i32 = 10;
pub(crate) const CS_QUANTITY_MAX: &str = "100";
pub(crate) const CS_MARKUP_MAX: &str = "2.00";
pub(crate) const CS_DISCOUNT_MAX: &str = "1.00";
pub(crate) const CS_WHOLESALE_MAX: &str = "100.00";
pub(crate) const CS_COUPON_MAX: &str = "0.50";
pub(crate) const CS_MIN_SHIP_DELAY: i32 = 2;
pub(crate) const CS_MAX_SHIP_DELAY: i32 = 90;
pub(crate) const CS_GIFT_PCT: i32 = 10;

pub(crate) const CURRENT_YEAR: i32 = 2003;
pub(crate) const CURRENT_MONTH: i32 = 1;
pub(crate) const CURRENT_DAY: i32 = 8;
pub(crate) const CURRENT_QUARTER: i32 = 1;
pub(crate) const CURRENT_WEEK: i32 = 2;
pub(crate) const DATE_MINIMUM: &str = "1998-01-01";
pub(crate) const DATE_MAXIMUM: &str = "2002-12-31";
pub(crate) const YEAR_MINIMUM: i32 = 1998;
pub(crate) const YEAR_MAXIMUM: i32 = 2002;
pub(crate) const TODAYS_DATE: &str = "2003-01-08";

pub(crate) const INV_QUANTITY_MIN: i32 = 0;
pub(crate) const INV_QUANTITY_MAX: i32 = 1000;

pub(crate) const PROMO_NAME_LEN: i32 = 5;
pub(crate) const PROMO_START_MIN: i32 = -720;
pub(crate) const PROMO_START_MAX: i32 = 100;
pub(crate) const PROMO_START_MEAN: i32 = 0;
pub(crate) const PROMO_LEN_MIN: i32 = 1;
pub(crate) const PROMO_LEN_MAX: i32 = 60;
pub(crate) const PROMO_LEN_MEAN: i32 = 0;
pub(crate) const PROMO_DETAIL_LEN_MIN: i32 = 20;
pub(crate) const PROMO_DETAIL_LEN_MAX: i32 = 60;

pub(crate) const SR_RETURN_PCT: i32 = 10;
pub(crate) const SS_MIN_SHIP_DELAY: i32 = 2;
pub(crate) const SS_MAX_SHIP_DELAY: i32 = 90;
pub(crate) const SS_QUANTITY_MAX: &str = "100";
pub(crate) const SS_MARKUP_MAX: &str = "1.00";
pub(crate) const SS_DISCOUNT_MAX: &str = "1.00";
pub(crate) const SS_WHOLESALE_MAX: &str = "100.00";
pub(crate) const SS_COUPON_MAX: &str = "0.50";

pub(crate) const WP_AUTOGEN_PCT: i32 = 30;
pub(crate) const WP_LINK_MIN: i32 = 2;
pub(crate) const WP_LINK_MAX: i32 = 25;
pub(crate) const WP_IMAGE_MIN: i32 = 1;
pub(crate) const WP_IMAGE_MAX: i32 = 7;
pub(crate) const WP_AD_MIN: i32 = 0;
pub(crate) const WP_AD_MAX: i32 = 4;
pub(crate) const WP_IDLE_TIME_MAX: i32 = 100;

pub(crate) const W_SQFT_MIN: i32 = 50000;
pub(crate) const W_SQFT_MAX: i32 = 1000000;
pub(crate) const W_NAME_MIN: i32 = 10;

pub(crate) const WR_RETURN_PCT: i32 = 10;
pub(crate) const WEB_START_DATE: &str = DATE_MINIMUM;
pub(crate) const WEB_END_DATE: &str = DATE_MAXIMUM;
pub(crate) const WEB_DATE_STAGGER: i64 = 17;
pub(crate) const WEB_PAGES_PER_SITE: i32 = 123;
pub(crate) const WEB_MORTALITY: i64 = 50;

/// `WEB_IS_REPLACED(j)`
pub(crate) fn web_is_replaced(j: i64) -> bool {
    (j % (100 / WEB_MORTALITY)) == 0
}

/// `WEB_IS_REPLACEMENT(j)`
pub(crate) fn web_is_replacement(j: i64) -> bool {
    ((j / (100 / WEB_MORTALITY)) % 2) != 0
}

pub(crate) const RS_CC_MARKET_CLASS: i32 = 50;
pub(crate) const RS_CC_MARKET_DESC: i32 = 100;
pub(crate) const RS_CC_DIVISION_NAME: i32 = 50;
pub(crate) const RS_CP_DESCRIPTION: i32 = 100;
pub(crate) const RS_I_ITEM_DESC: i32 = 200;
pub(crate) const RS_I_MANUFACT: i32 = 50;
pub(crate) const RS_I_FORMULATION: i32 = 20;
pub(crate) const RS_I_PRODUCT_NAME: i32 = 50;
pub(crate) const RS_S_MARKET_DESC: i32 = 100;
pub(crate) const RS_SM_CONTRACT: i32 = 20;
pub(crate) const RS_W_WAREHOUSE_NAME: i32 = 20;
pub(crate) const RS_WEB_MARKET_CLASS: i32 = 50;
pub(crate) const RS_WEB_MARKET_DESC: i32 = 100;
pub(crate) const RS_WEB_COMPANY_NAME: i32 = 100;

// w_call_center.h
pub(crate) const MIN_CC_TAX_PERCENTAGE: &str = "0.00";
pub(crate) const MAX_CC_TAX_PERCENTAGE: &str = "0.12";
// w_customer_demographics.h
pub(crate) const CD_MAX_CHILDREN: i64 = 7;
pub(crate) const CD_MAX_EMPLOYED: i64 = 7;
pub(crate) const CD_MAX_COLLEGE: i64 = 7;
// w_item.h
pub(crate) const I_PROMO_PERCENTAGE: i32 = 20;
pub(crate) const MIN_ITEM_MARKDOWN_PCT: &str = "0.30";
pub(crate) const MAX_ITEM_MARKDOWN_PCT: &str = "0.90";
// w_store.h
pub(crate) const STORE_MIN_TAX_PERCENTAGE: &str = "0.00";
pub(crate) const STORE_MAX_TAX_PERCENTAGE: &str = "0.11";
pub(crate) const STORE_MIN_DAYS_OPEN: i32 = 5;
pub(crate) const STORE_MAX_DAYS_OPEN: i32 = 500;
pub(crate) const STORE_CLOSED_PCT: i32 = 30;
pub(crate) const STORE_DESC_MIN: i32 = 15;
// w_web_site.h
pub(crate) const WEB_MIN_TAX_PERCENTAGE: &str = "0.00";
pub(crate) const WEB_MAX_TAX_PERCENTAGE: &str = "0.12";
// w_web_sales.h
pub(crate) const WS_QUANTITY_MAX: &str = "100";
pub(crate) const WS_MARKUP_MAX: &str = "2.00";
pub(crate) const WS_DISCOUNT_MAX: &str = "1.00";
pub(crate) const WS_WHOLESALE_MAX: &str = "100.00";
pub(crate) const WS_COUPON_MAX: &str = "0.50";
pub(crate) const WS_GIFT_PCT: i32 = 7;
pub(crate) const WS_MIN_SHIP_DELAY: i32 = 1;
pub(crate) const WS_MAX_SHIP_DELAY: i32 = 120;
// w_store_returns.h
pub(crate) const SR_SAME_CUSTOMER: i32 = 80;

// dist.h
pub(crate) const DIST_UNIFORM: i32 = 0x0001;
pub(crate) const DIST_EXPONENTIAL: i32 = 0x0002;
pub(crate) const DIST_SALES: i32 = 3;
pub(crate) const DIST_RETURNS: i32 = 5;

// tpcds.idx.h
pub(crate) const CALENDAR_UNIFORM: i32 = 1;
pub(crate) const CALENDAR_SALES: i32 = 3;

// genrand.h
pub(crate) const ALPHANUM: &[u8] = b"abcdefghijklmnopqrstuvxyzABCDEFGHIJKLMNOPQRSTUVXYZ0123456789";
pub(crate) const DIGITS: &[u8] = b"0123456789";
pub(crate) const RNG_SEED: i64 = 19620718;

// porting.h / config.h
pub(crate) const MAXINT: i64 = i32::MAX as i64;
