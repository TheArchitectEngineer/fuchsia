// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{ap as ap_sme, MlmeEventStream, MlmeSink, MlmeStream};
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::prelude::*;
use futures::select;
use ieee80211::Ssid;
use log::error;
use std::pin::pin;
use std::sync::Arc;
use wlan_common::RadioConfig;
use {fidl_fuchsia_wlan_mlme as fidl_mlme, fidl_fuchsia_wlan_sme as fidl_sme};

pub type Endpoint = fidl::endpoints::ServerEnd<fidl_sme::ApSmeMarker>;
type Sme = ap_sme::ApSme;

pub fn serve(
    device_info: fidl_mlme::DeviceInfo,
    event_stream: MlmeEventStream,
    new_fidl_clients: mpsc::UnboundedReceiver<Endpoint>,
) -> (MlmeSink, MlmeStream, impl Future<Output = Result<(), anyhow::Error>>) {
    let (sme, mlme_sink, mlme_stream, time_stream) = Sme::new(device_info);
    let fut = async move {
        let sme = Arc::new(Mutex::new(sme));
        let mlme_sme = super::serve_mlme_sme(event_stream, Arc::clone(&sme), time_stream);
        let sme_fidl = super::serve_fidl(&*sme, new_fidl_clients, handle_fidl_request);
        let mlme_sme = pin!(mlme_sme);
        let sme_fidl = pin!(sme_fidl);
        select! {
            mlme_sme = mlme_sme.fuse() => mlme_sme?,
            sme_fidl = sme_fidl.fuse() => match sme_fidl? {},
        }
        Ok(())
    };
    (mlme_sink, mlme_stream, fut)
}

async fn handle_fidl_request(
    sme: &Mutex<Sme>,
    request: fidl_sme::ApSmeRequest,
) -> Result<(), ::fidl::Error> {
    match request {
        fidl_sme::ApSmeRequest::Start { config, responder } => {
            let r = start(sme, config).await;
            responder.send(r)?;
        }
        fidl_sme::ApSmeRequest::Stop { responder } => {
            let r = stop(sme).await;
            responder.send(r)?;
        }
        fidl_sme::ApSmeRequest::Status { responder } => {
            let r = status(sme);
            responder.send(&r)?;
        }
    }
    Ok(())
}

async fn start(sme: &Mutex<Sme>, config: fidl_sme::ApConfig) -> fidl_sme::StartApResultCode {
    let radio_cfg = match RadioConfig::try_from(config.radio_cfg) {
        Ok(radio_cfg) => radio_cfg,
        Err(e) => {
            error!("Could not convert RadioConfig from ApConfig: {:?}", e);
            return fidl_sme::StartApResultCode::InternalError;
        }
    };

    let sme_config = ap_sme::Config {
        ssid: Ssid::from_bytes_unchecked(config.ssid),
        password: config.password,
        radio_cfg,
    };

    let receiver = sme.lock().on_start_command(sme_config);
    let r = receiver.await.unwrap_or_else(|_| {
        error!("Responder for AP Start command was dropped without sending a response");
        ap_sme::StartResult::InternalError
    });

    match r {
        ap_sme::StartResult::Success => fidl_sme::StartApResultCode::Success,
        ap_sme::StartResult::AlreadyStarted => fidl_sme::StartApResultCode::AlreadyStarted,
        ap_sme::StartResult::InternalError => fidl_sme::StartApResultCode::InternalError,
        ap_sme::StartResult::Canceled => fidl_sme::StartApResultCode::Canceled,
        ap_sme::StartResult::TimedOut => fidl_sme::StartApResultCode::TimedOut,
        ap_sme::StartResult::PreviousStartInProgress => {
            fidl_sme::StartApResultCode::PreviousStartInProgress
        }
        ap_sme::StartResult::InvalidArguments(e) => {
            error!("Invalid arguments for AP start: {}", e);
            fidl_sme::StartApResultCode::InvalidArguments
        }
    }
}

async fn stop(sme: &Mutex<Sme>) -> fidl_sme::StopApResultCode {
    let receiver = sme.lock().on_stop_command();
    receiver.await.unwrap_or_else(|_| {
        error!("Responder for AP Stop command was dropped without sending a response");
        fidl_sme::StopApResultCode::InternalError
    })
}

fn status(sme: &Mutex<Sme>) -> fidl_sme::ApStatusResponse {
    fidl_sme::ApStatusResponse { running_ap: sme.lock().get_running_ap().map(Box::new) }
}
