//! Pre-allocation resident and temporary byte ceilings.

use super::StoreError;

#[derive(Clone, Copy)]
pub(crate) struct Budgets {
    resident: u64,
    temporary: u64,
}

impl Budgets {
    pub(crate) const fn new(resident: u64, temporary: u64) -> Self {
        Self {
            resident,
            temporary,
        }
    }

    pub(crate) fn check(
        self,
        resident_now: u64,
        temporary_now: u64,
        bytes: u64,
        component: &'static str,
        is_temporary: bool,
    ) -> Result<(u64, u64), StoreError> {
        let needed_resident =
            resident_now
                .checked_add(bytes)
                .ok_or(StoreError::BudgetExceeded {
                    needed: u64::MAX,
                    budget: self.resident,
                    component,
                })?;
        let needed_temporary = if is_temporary {
            temporary_now
                .checked_add(bytes)
                .ok_or(StoreError::BudgetExceeded {
                    needed: u64::MAX,
                    budget: self.temporary,
                    component,
                })?
        } else {
            temporary_now
        };
        if is_temporary && needed_temporary > self.temporary {
            return Err(StoreError::BudgetExceeded {
                needed: needed_temporary,
                budget: self.temporary,
                component,
            });
        }
        if needed_resident > self.resident {
            return Err(StoreError::BudgetExceeded {
                needed: needed_resident,
                budget: self.resident,
                component,
            });
        }
        Ok((needed_resident, needed_temporary))
    }
}
