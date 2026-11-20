use {
    crate::{
        domain::{
            competition::{
                Auction,
                Solution,
                auction,
                solution::{
                    Error,
                    Id,
                    Interaction,
                    Settlement,
                    interaction::Custom,
                    settlement::Internalization,
                },
            },
            mempools::SubmissionMode,
        },
        infra::{
            Ethereum,
            Solver,
            blockchain::{Rpc, RpcArgs, gas::GasPriceParameters},
            config::file::GasEstimatorType,
            solver::{Account, Config},
        },
    },
    alloy::{
        primitives::{Address, B256, U256},
        rpc::types::Block,
        sol_types::SolCall,
    },
    axum::{Json, Router, extract::State, routing::post},
    contracts::{
        GPv2Settlement::GPv2Settlement::settleCall,
        support::DeadlineCheck::DeadlineCheck::checkDeadlineCall,
    },
    eth_domain_types as eth,
    serde_json::{Value, json},
    std::{
        sync::{Arc, Mutex},
        time::Duration,
    },
    tokio::net::TcpListener,
};

type Calls = Arc<Mutex<Vec<Value>>>;

const UNFUNDED_SOLVER: Address = Address::repeat_byte(0xaa);

async fn rpc(State(calls): State<Calls>, Json(request): Json<Value>) -> Json<Value> {
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" => json!("0x1"),
        "eth_getBlockByNumber" => {
            let mut block: Block = Default::default();
            block.header.hash = B256::repeat_byte(0x55);
            block.header.inner.number = 1;
            block.header.inner.gas_limit = 30_000_000;
            block.header.inner.base_fee_per_gas = Some(1);
            json!(block)
        }
        "eth_call" => json!(B256::ZERO),
        "eth_gasPrice" => json!("0x2"),
        "eth_estimateGas" => json!("0xea60"),
        "eth_getBalance" if request["params"][0] == json!(UNFUNDED_SOLVER) => json!("0x0"),
        "eth_getBalance" => json!("0xde0b6b3a7640000"),
        other => panic!("unexpected RPC method {other}"),
    };
    calls.lock().unwrap().push(request.clone());
    Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
}

fn assert_revalidated(calls: &[Value], sender: Address, internalized: &eth::Tx, full: &eth::Tx) {
    let gas: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "eth_estimateGas")
        .map(|call| &call["params"][0])
        .collect();
    assert_eq!(gas.len(), 2);
    for (request, prepared) in gas.into_iter().zip([internalized, full]) {
        assert_eq!(request["from"], json!(sender));
        assert_eq!(request["to"], json!(prepared.to));
        let input = request
            .get("input")
            .or_else(|| request.get("data"))
            .unwrap();
        assert_eq!(*input, json!(prepared.input));
    }
    let balances: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "eth_getBalance")
        .map(|call| &call["params"][0])
        .collect();
    assert_eq!(balances, [&json!(sender)]);
}

#[tokio::test]
async fn prepared_settlement_reuses_payer_and_revalidates_a_new_one() {
    let calls = Calls::default();
    let app = Router::new()
        .route("/", post(rpc))
        .with_state(calls.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let rpc = Rpc::try_new(RpcArgs {
        url,
        max_batch_size: 0,
        max_concurrent_requests: 0,
    })
    .await
    .unwrap();
    let eth = Ethereum::new(
        rpc.clone(),
        Default::default(),
        &shared::current_block::Arguments {
            block_stream_poll_interval: Some(Duration::from_secs(3600)),
            node_ws_url: None,
        },
        8_000_000.into(),
        &GasEstimatorType::Web3,
        GasPriceParameters {
            max_additional_tip: U256::ZERO,
            additional_tip_factor: 0.0,
            max_fee_per_gas: U256::MAX,
            min_priority_fee: U256::ZERO,
        },
    )
    .await;
    let simulator = simulator::Simulator::ethereum(simulator::Ethereum::new(
        rpc.web3().clone(),
        rpc.chain(),
        Default::default(),
        eth.gas_estimator(),
        eth.current_block().clone(),
        U256::from(8_000_000),
    ));
    let solver = Solver::try_new(
        Config {
            accounts: vec![Account::Address(UNFUNDED_SOLVER)],
            ..Default::default()
        },
        eth.clone(),
    )
    .await
    .unwrap();
    let native = solver.solver_native_token();
    let auction = Auction::new(
        auction::Kind::Competition(auction::Id(7)),
        vec![],
        std::iter::empty(),
        chrono::Utc::now(),
        &eth,
        Default::default(),
    )
    .await
    .unwrap();
    let solution = Solution::new(
        Id::new(3),
        vec![],
        Default::default(),
        vec![],
        vec![Interaction::Custom(Custom {
            target: Address::repeat_byte(0x33).into(),
            value: 0.into(),
            call_data: vec![0x44].into(),
            allowances: vec![],
            inputs: vec![],
            outputs: vec![],
            internalize: true,
        })],
        vec![],
        solver,
        eth.contracts().weth_address(),
        None,
        None,
        None,
        None,
        &Default::default(),
        Default::default(),
        vec![],
    )
    .unwrap();

    let original = Address::repeat_byte(0x11);
    let replacement = Address::repeat_byte(0x22);
    calls.lock().unwrap().clear();
    let mut exact_solution = solution.clone();
    let prepared = solution
        .encode_from(&auction, &eth, &simulator, native, original)
        .await
        .unwrap();
    let direct = prepared.transaction(Internalization::Enable);
    let full = prepared.transaction(Internalization::Disable);
    assert_ne!(direct.input, full.input);
    for (mode, transaction) in [
        (Internalization::Enable, direct),
        (Internalization::Disable, full),
    ] {
        assert_eq!(prepared.protocol_calldata(mode), transaction.input);
    }
    assert_revalidated(
        &std::mem::take(&mut *calls.lock().unwrap()),
        original,
        direct,
        full,
    );

    let unchanged = prepared
        .for_sender(original, &eth, &simulator)
        .await
        .unwrap();
    assert_eq!(
        unchanged.transaction(Internalization::Enable).input,
        direct.input
    );
    assert!(calls.lock().unwrap().is_empty());

    let switched = prepared
        .for_sender(replacement, &eth, &simulator)
        .await
        .unwrap();
    let new_direct = switched.transaction(Internalization::Enable);
    let new_full = switched.transaction(Internalization::Disable);
    assert_eq!(new_direct.from, replacement);
    assert_eq!(new_full.from, replacement);
    assert_eq!(new_direct.input, direct.input);
    assert_eq!(new_full.input, full.input);
    assert_revalidated(
        &std::mem::take(&mut *calls.lock().unwrap()),
        replacement,
        new_direct,
        new_full,
    );

    let no_deadline = switched.encode(&eth, None).unwrap();
    assert_eq!(no_deadline.internalized.input, direct.input);
    for deadline in [101, 102] {
        let encoded = switched.encode(&eth, Some(eth::BlockNo(deadline))).unwrap();
        for tx in [encoded.internalized, encoded.uninternalized] {
            assert_eq!(tx.from, replacement);
            let settle = settleCall::abi_decode(&tx.input).unwrap();
            let guard = &settle.interactions[0][0];
            assert_eq!(guard.target, *eth.contracts().deadline_check().address());
            assert_eq!(
                checkDeadlineCall::abi_decode(&guard.callData)
                    .unwrap()
                    .deadline,
                U256::from(deadline)
            );
        }
    }
    assert!(calls.lock().unwrap().is_empty());

    assert!(matches!(
        exact_solution
            .clone()
            .encode_from(&auction, &eth, &simulator, native, UNFUNDED_SOLVER)
            .await,
        Err(Error::SolverAccountInsufficientBalance(_))
    ));
    calls.lock().unwrap().clear();

    let delegated_mode = |submitter_eoa| SubmissionMode::Delegated {
        submitter_eoa,
        solver_eoa: UNFUNDED_SOLVER,
    };
    let assert_delegated = |candidate: &Settlement, payer| {
        let wrapped = candidate.transaction(Internalization::Enable);
        let wrapped_full = candidate.transaction(Internalization::Disable);
        for (mode, wrapper, canonical) in [
            (Internalization::Enable, wrapped, direct),
            (Internalization::Disable, wrapped_full, full),
        ] {
            assert_eq!(candidate.protocol_calldata(mode), canonical.input);
            assert_eq!(wrapper.from, payer);
            assert_eq!(wrapper.to, UNFUNDED_SOLVER);
            assert_eq!(&wrapper.input[..20], canonical.to.as_slice());
            assert_eq!(&wrapper.input[20..], canonical.input.as_ref());
        }
        assert_revalidated(
            &std::mem::take(&mut *calls.lock().unwrap()),
            payer,
            wrapped,
            wrapped_full,
        );
    };
    let delegated = exact_solution
        .clone()
        .encode_for_submission(&auction, &eth, &simulator, native, delegated_mode(original))
        .await
        .unwrap();
    assert_delegated(&delegated, original);
    let redelegated = delegated
        .for_submission(delegated_mode(replacement), &eth, &simulator)
        .await
        .unwrap();
    assert_delegated(&redelegated, replacement);

    // The same signer must be validated again when the destination and calldata
    // change.
    let reverted_to_direct = redelegated
        .for_sender(replacement, &eth, &simulator)
        .await
        .unwrap();
    let restored = reverted_to_direct.transaction(Internalization::Enable);
    let restored_full = reverted_to_direct.transaction(Internalization::Disable);
    assert_eq!(restored.to, direct.to);
    assert_eq!(restored.input, direct.input);
    assert_eq!(restored_full.to, full.to);
    assert_eq!(restored_full.input, full.input);
    assert_revalidated(
        &std::mem::take(&mut *calls.lock().unwrap()),
        replacement,
        restored,
        restored_full,
    );

    let mut state = alloy::rpc::types::state::StateOverride::default();
    state.insert(Address::repeat_byte(0x88), Default::default());
    let target = eth::BlockNo(2);
    let timestamp = chrono::Utc::now().timestamp().unsigned_abs() + 300;
    let context = crate::domain::competition::solution::SimulationContext::new(
        1,
        B256::repeat_byte(0x55),
        target.0,
        timestamp,
        state,
    )
    .unwrap();
    exact_solution.set_simulation_context(context).unwrap();
    let exact = exact_solution
        .encode_for_submission(
            &auction,
            &eth,
            &simulator,
            native,
            delegated_mode(replacement),
        )
        .await
        .unwrap();
    let encoded = exact.encode(&eth, None).unwrap();
    assert_eq!(
        encoded.internalized.input,
        exact.transaction(Internalization::Enable).input
    );
    assert_eq!(
        encoded.uninternalized.input,
        exact.transaction(Internalization::Disable).input
    );
    assert_eq!(exact.submission_deadline(eth::BlockNo(1)), None);
    assert_eq!(exact.submission_deadline(eth::BlockNo(7)), Some(target));
    let encoded = exact.encode(&eth, Some(eth::BlockNo(7))).unwrap();
    for tx in [encoded.internalized, encoded.uninternalized] {
        assert_eq!(tx.from, replacement);
        assert_eq!(tx.to, UNFUNDED_SOLVER);
        assert_eq!(&tx.input[..20], direct.to.as_slice());
        let settle = settleCall::abi_decode(&tx.input[20..]).unwrap();
        let guard = checkDeadlineCall::abi_decode(&settle.interactions[0][0].callData).unwrap();
        assert_eq!(guard.deadline, U256::from(target.0));
    }
    let calls = calls.lock().unwrap();
    let estimates: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "eth_estimateGas")
        .collect();
    assert_eq!(estimates.len(), 2);
    for call in estimates {
        let params = &call["params"];
        assert_eq!(params[1]["blockHash"], json!(B256::repeat_byte(0x55)));
        assert_eq!(params[1]["requireCanonical"], true);
        assert_eq!(
            params[2],
            json!({ (Address::repeat_byte(0x88).to_string().to_lowercase()): {} })
        );
        assert_eq!(params[3]["number"], "0x2");
        assert_eq!(params[3]["time"], json!(format!("0x{timestamp:x}")));
    }
    server.abort();
}
