#[cfg(creusot)]
use creusot_std::prelude::*;

/// Counts whole serving units on one node, or rejects invalid headroom.
///
/// `None` means that the unit size is zero or that the free and reclaimable
/// GPUs together exceed the physical node capacity (including addition
/// overflow). All arithmetic in the contracts uses mathematical integers.
#[cfg_attr(
    creusot,
    ensures(
        (result == None)
            == (gpus_per_unit@ == 0
                || free_gpus@ + reclaimable_gpus@ > physical_gpus@)
    )
)]
#[cfg_attr(
    creusot,
    ensures(
        free_gpus@ + reclaimable_gpus@ > u32::MAX@ ==> result == None
    )
)]
#[cfg_attr(
    creusot,
    ensures(match result {
        Some(units) => gpus_per_unit@ > 0
            && free_gpus@ + reclaimable_gpus@ <= physical_gpus@
            && units@ == (free_gpus@ + reclaimable_gpus@) / gpus_per_unit@,
        None => true,
    })
)]
#[cfg_attr(
    creusot,
    ensures(match result {
        Some(units) => units@ * gpus_per_unit@ <= free_gpus@ + reclaimable_gpus@,
        None => true,
    })
)]
#[cfg_attr(
    creusot,
    ensures(match result {
        Some(units) => free_gpus@ + reclaimable_gpus@ < (units@ + 1) * gpus_per_unit@,
        None => true,
    })
)]
pub(crate) fn node_serving_units(
    physical_gpus: u32,
    free_gpus: u32,
    reclaimable_gpus: u32,
    gpus_per_unit: u32,
) -> Option<u32> {
    if gpus_per_unit == 0 {
        return None;
    }
    let available = free_gpus.checked_add(reclaimable_gpus)?;
    if available > physical_gpus {
        return None;
    }
    Some(available / gpus_per_unit)
}

#[cfg(test)]
mod tests {
    use super::node_serving_units;

    #[test]
    fn zero_unit_size_is_rejected() {
        assert_eq!(node_serving_units(8, 0, 0, 0), None);
        assert_eq!(node_serving_units(8, 4, 4, 0), None);
    }
}
