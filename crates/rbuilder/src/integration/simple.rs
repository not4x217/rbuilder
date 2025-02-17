#[cfg(test)]
mod tests {
    use crate::{
        beacon_api_client::Client as BeaconApiClient,
        mev_boost::RelayClient,
        primitives::mev_boost::MevBoostRelaySlotInfoProvider,
        live_builder::{
            block_list_provider::{
                NullBlockListProvider,
                test::{BlocklistHttpServer, BLOCKLIST_LEN_2}
            },
            payload_events::{MevBoostSlotDataGenerator, MevBoostSlotData},
        },
        integration::playground::Playground,
    };

    use alloy_network::TransactionBuilder;
    use alloy_primitives::{U256, Bytes};
    use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use alloy_rpc_types_mev::EthSendBundle;
    use alloy_rpc_client::{ClientBuilder, RpcCall};
    use alloy_eips::eip2718::Encodable2718;
    use std::{path::PathBuf, str::FromStr, time::Duration, sync::Arc};
    use test_utils::ignore_if_env_not_set;
    use url::Url;

    async fn send_transaction(
        srv: &Playground,
        private_key: alloy_network::EthereumWallet,
        to: Option<alloy_primitives::Address>,
    ) -> eyre::Result<alloy_primitives::TxHash> {
        let rbuilder_provider =
            ProviderBuilder::new().on_http(Url::parse(srv.rbuilder_rpc_url()).unwrap());

        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(private_key)
            .on_http(Url::parse(srv.el_url()).unwrap());

        let gas_price = provider.get_gas_price().await?;

        let tx = TransactionRequest::default()
            .with_to(to.unwrap_or(srv.builder_address()))
            .with_value(U256::from_str("10000000000000000000").unwrap())
            .with_gas_price(gas_price)
            .with_gas_limit(21000);

        let tx = provider.fill(tx).await?;

        // send the transaction ONLY to the builder
        let pending_tx = rbuilder_provider
            .send_tx_envelope(tx.as_envelope().unwrap().clone())
            .await?;

        Ok(*pending_tx.tx_hash())
    }

    #[ignore_if_env_not_set("PLAYGROUND")] // TODO: Change with a custom macro (i.e ignore_if_not_playground)
    #[tokio::test]
    async fn test_simple_example() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rbuilder/src/integration/test_data/config-playground.toml");

        // This test sends a transaction ONLY to the builder and waits for the block to be built with it.
        let srv = Playground::new(&config_path).unwrap();
        srv.wait_for_next_slot().await.unwrap();

        // Send transaction using the helper function
        let tx_hash = send_transaction(&srv, srv.prefunded_key(), None)
            .await
            .unwrap();

        // Wait for receipt
        let binding = ProviderBuilder::new().on_http(Url::parse(srv.el_url()).unwrap());
        let pending_tx = PendingTransactionBuilder::new(binding.clone(), tx_hash)
            .with_timeout(Some(std::time::Duration::from_secs(60)));

        let receipt = pending_tx.get_receipt().await.unwrap();
        srv.validate_block_built(receipt.block_number.unwrap())
            .await
            .unwrap();

        // Send a transaction with an account from the blocklist
        // TODO: This should be a separated test but the integration framework does use fixed port numbers
        // and we need to change it to use dynamic ports.
        // Since we only send the transaction to the builder, it should never be included in the block.
        {
            srv.wait_for_next_slot().await.unwrap();
            let tx_hash = send_transaction(&srv, srv.blocklist_key(), None)
                .await
                .unwrap();

            // wait for 20 seconds
            let pending_tx = PendingTransactionBuilder::new(binding.clone(), tx_hash)
                .with_timeout(Some(std::time::Duration::from_secs(20)));

            assert!(
                pending_tx.get_receipt().await.is_err(),
                "Expected transaction to fail since account is blocklisted"
            );
        }

        // Second blocklist test, send a transaction from a non-blocklisted account to a blocklisted account
        {
            srv.wait_for_next_slot().await.unwrap();
            let tx_hash =
                send_transaction(&srv, srv.prefunded_key(), Some(srv.blocklist_address()))
                    .await
                    .unwrap();

            // wait for 20 seconds
            let pending_tx = PendingTransactionBuilder::new(binding, tx_hash)
                .with_timeout(Some(std::time::Duration::from_secs(20)));

            assert!(
                pending_tx.get_receipt().await.is_err(),
                "Expected transaction to fail since account is blocklisted"
            );
        }
    }

    #[ignore_if_env_not_set("PLAYGROUND")]
    /// TODO: Change with a custom macro (i.e ignore_if_not_playground)
    /// Sadly builder shutdown does not always work properly so we have to wait for the watchdog to kill the process.
    #[tokio::test]
    async fn test_builder_closes_on_old_blocklist() {
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../crates/rbuilder/src/integration/test_data/config-playground-http-blocklist.toml",
        );
        let blocklist_server = BlocklistHttpServer::new(1934, Some(BLOCKLIST_LEN_2.to_string()));
        tokio::time::sleep(Duration::from_millis(100)).await; //puaj
        let mut srv = Playground::new(&config_path).unwrap();
        srv.wait_for_next_slot().await.unwrap();
        blocklist_server.set_answer(None);
        let timeout_secs = 5 /*blocklist_url_max_age_secs in cfg */ +
             12 /* problem detected in next block start an cancel is signaled*/+
             15 /*watchdog_timeout_sec */+
             12 /*extra delay from watchdog*/+
             1 /* for timing errors */;
        tokio::time::sleep(Duration::from_secs(timeout_secs)).await; //puaj
        assert!(!srv.builder_is_alive());
    }

    async fn send_single_tx_bundle(
        srv: &Playground,
        private_key: alloy_network::EthereumWallet,
        to: Option<alloy_primitives::Address>,
        slot_data: MevBoostSlotData,
    ) -> eyre::Result<alloy_primitives::TxHash> {
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(private_key)
            .on_http(Url::parse(srv.el_url()).unwrap());

        let gas_price = provider.get_gas_price().await?;

        let tx = TransactionRequest::default()
            .with_to(to.unwrap_or(srv.builder_address()))
            .with_value(U256::from_str("10000000000000000000").unwrap())
            .with_gas_price(gas_price)
            .with_gas_limit(21000);
        let tx = provider.fill(tx).await?;
        let tx = tx.as_envelope().unwrap();
        let tx_encoded: Bytes = tx.encoded_2718().into();

        let bundle = EthSendBundle{
            txs: vec![tx_encoded],
            block_number: slot_data.block() + 1,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            replacement_uuid: None,
        };

        let rpc = ClientBuilder::default().
            http(Url::parse(srv.rbuilder_rpc_url()).unwrap());
        let send_req: RpcCall<_, _, Option<String>> = rpc.request("eth_sendBundle", (&bundle,));
        send_req.await?;

        Ok(*tx.tx_hash())
    }

    #[ignore_if_env_not_set("PLAYGROUND")] // TODO: Change with a custom macro (i.e ignore_if_not_playground)
    #[tokio::test]
    async fn test_bundle() {
        // Start playground
        let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rbuilder/src/integration/test_data/config-playground.toml");
        let srv = Playground::new(&config_path).unwrap();
            srv.wait_for_next_slot().await.unwrap();

        // Create cl/relay clients
        let cl_clients = vec![BeaconApiClient::new(Url::parse(srv.cl_url()).unwrap())];
        let relay_client = RelayClient::from_url(Url::parse(srv.relay_url()).unwrap(), None, None, None);
        let slot_info_providers = vec![MevBoostRelaySlotInfoProvider::new(relay_client, String::default(), 0)];
        let slot_source = MevBoostSlotDataGenerator::new(
            cl_clients, 
            slot_info_providers,
            Arc::new(NullBlockListProvider::new()),
            Default::default(),
        );

        // Create bundle with sinle transfer for each new slot but after 10 seconds last tx has been confirmed
        let mut now = std::time::Instant::now();
        let (_handle, mut slot_data_chan) = slot_source.spawn();        
        loop {
            let mut slot_data = Vec::<MevBoostSlotData>::new();
            tokio::select! {
                slot_count = slot_data_chan.recv_many(&mut slot_data, 10000) => {                    
                    // Wait 10 seconds from last tx confirmation
                    if now.elapsed().as_secs() < 10 {
                        continue;
                    }
                    if slot_count == 0 {
                        continue
                    }

                    // Create bundle with single transaction
                    let tx_hash = send_single_tx_bundle(
                        &srv,
                        srv.prefunded_key(),
                        None,
                        slot_data.last().unwrap().clone(),
                    ).await.unwrap();

                    // Wait for receipt for tx from bundle
                    let binding = ProviderBuilder::new().on_http(Url::parse(srv.el_url()).unwrap());
                    let pending_tx = PendingTransactionBuilder::new(binding.clone(), tx_hash)
                        .with_timeout(Some(std::time::Duration::from_secs(60)));
                    if let Err(err) = pending_tx.get_receipt().await {
                        println!("transaction from bundle not confirmed - {:?}", err.to_string());
                    }

                    now = std::time::Instant::now();
                }
            }
        }
    }
}
