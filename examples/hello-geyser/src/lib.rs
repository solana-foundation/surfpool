use agave_geyser_plugin_interface::geyser_plugin_interface::{
    GeyserPlugin, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions, ReplicaEntryInfoVersions,
    ReplicaTransactionInfoVersions, Result as PluginResult, SlotStatus,
};
use solana_clock::{BankId, Slot};

#[derive(Default, Debug)]
pub struct HelloGeyserPlugin {}

impl GeyserPlugin for HelloGeyserPlugin {
    fn name(&self) -> &'static str {
        "hello-geyser"
    }

    fn on_load(&mut self, _config_file: &str, _is_reload: bool) -> PluginResult<()> {
        Ok(())
    }

    fn on_unload(&mut self) {}

    fn notify_end_of_startup(&self) -> PluginResult<()> {
        Ok(())
    }

    fn update_account_from_snapshot(
        &self,
        account: ReplicaAccountInfoVersions,
        _slot: Slot,
    ) -> PluginResult<()> {
        print_lamports(account);
        Ok(())
    }

    fn update_account_for_bank(
        &self,
        account: ReplicaAccountInfoVersions,
        _slot: Slot,
        _bank_id: BankId,
    ) -> PluginResult<()> {
        print_lamports(account);
        Ok(())
    }

    fn update_bank_status(
        &self,
        _slot: Slot,
        _parent: Option<u64>,
        _status: &SlotStatus,
        _bank_id: BankId,
    ) -> PluginResult<()> {
        Ok(())
    }

    fn notify_transaction_for_bank(
        &self,
        _transaction: ReplicaTransactionInfoVersions,
        slot: Slot,
        _bank_id: BankId,
    ) -> PluginResult<()> {
        println!("notifying of transaction at slot: {}", slot);
        Ok(())
    }

    fn notify_entry_for_bank(
        &self,
        _entry: ReplicaEntryInfoVersions,
        _bank_id: BankId,
    ) -> PluginResult<()> {
        Ok(())
    }

    fn notify_block_metadata_for_bank(
        &self,
        _blockinfo: ReplicaBlockInfoVersions,
        _bank_id: BankId,
    ) -> PluginResult<()> {
        Ok(())
    }

    fn account_data_notifications_enabled(&self) -> bool {
        true
    }

    fn transaction_notifications_enabled(&self) -> bool {
        false
    }

    fn entry_notifications_enabled(&self) -> bool {
        false
    }
}

fn print_lamports(account: ReplicaAccountInfoVersions) {
    let account_info = match account {
        ReplicaAccountInfoVersions::V0_0_1(_info) => {
            unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
        }
        ReplicaAccountInfoVersions::V0_0_2(_info) => {
            unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported")
        }
        ReplicaAccountInfoVersions::V0_0_3(info) => info,
    };

    println!("lamports: {}", account_info.lamports);
}

#[no_mangle]
#[allow(improper_ctypes_definitions)]

/// # Safety
///
/// This function returns the Plugin pointer as trait GeyserPlugin.
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    let plugin: Box<dyn GeyserPlugin> = Box::<HelloGeyserPlugin>::default();
    Box::into_raw(plugin)
}
