// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_inventory.cpp @ rust-port-baseline

//! The `inventory` table: every item in every warehouse, once a week.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::date::strtodt;
use super::tables::*;
use crate::append_info::AppendInfo;

/// The statics of `mk_w_inventory`.
#[derive(Default, Clone, Copy)]
pub(crate) struct State {
    item_count: i64,
    warehouse_count: i64,
    j_date: i32,
}

/// `mk_w_inventory`
pub(crate) fn mk_w_inventory(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    if !ctx.init.mk_w_inventory_init {
        ctx.inventory.item_count = ctx.get_id_count(ITEM);
        ctx.inventory.warehouse_count = ctx.get_rowcount(WAREHOUSE);
        ctx.inventory.j_date = strtodt(DATE_MINIMUM).julian;
        ctx.init.mk_w_inventory_init = true;
    }
    let State {
        item_count,
        warehouse_count,
        j_date,
    } = ctx.inventory;
    ctx.null_set(INVENTORY, INV_NULLS);
    let mut n_temp = index as i32 - 1;
    let inv_item_sk = i64::from(n_temp) % item_count + 1;
    n_temp /= item_count as i32;
    let inv_warehouse_sk = i64::from(n_temp) % warehouse_count + 1;
    n_temp /= warehouse_count as i32;
    let inv_date_sk = i64::from(j_date + n_temp * 7);

    // the join between item and inventory is tricky: the item_id selected above identifies a
    // unique part num, but item is an SCD, so we need to account for that in selecting the SK
    // to join with
    let inv_item_sk = ctx.match_scd_sk(inv_item_sk, inv_date_sk, ITEM);
    let inv_quantity_on_hand = ctx.genrand_integer(
        DIST_UNIFORM,
        INV_QUANTITY_MIN,
        INV_QUANTITY_MAX,
        0,
        INV_QUANTITY_ON_HAND,
    );

    let info = info_arr.row_start(INVENTORY, ctx.tdef(INVENTORY).k_null_bit_map);
    info.append_key(inv_date_sk, INV_DATE_SK);
    info.append_key(inv_item_sk, INV_ITEM_SK);
    info.append_key(inv_warehouse_sk, INV_WAREHOUSE_SK);
    info.append_integer(inv_quantity_on_hand, INV_QUANTITY_ON_HAND);
    info.row_end();
}

/// `sc_w_inventory`: the inventory's row count.
pub(crate) fn sc_w_inventory(ctx: &mut Ctx) -> i64 {
    let mut k_res = ctx.get_id_count(ITEM);
    k_res *= ctx.get_rowcount(WAREHOUSE);
    let mut n_days = strtodt(DATE_MAXIMUM).julian;
    n_days -= strtodt(DATE_MINIMUM).julian;
    n_days += 1;
    n_days += 6;
    n_days /= 7; // each item's inventory is updated weekly
    k_res * i64::from(n_days)
}
