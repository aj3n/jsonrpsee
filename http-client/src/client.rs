use crate::traits::Client;
use crate::transport::HttpTransportClient;
use crate::v2::request::{JsonRpcCallSer, JsonRpcNotificationSer};
use crate::v2::{
	error::JsonRpcErrorAlloc,
	params::{Id, JsonRpcParams},
	response::JsonRpcResponse,
};
use crate::{Error, JsonRawValue, TEN_MB_SIZE_BYTES};
use async_trait::async_trait;
use fnv::FnvHashMap;
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicU64, Ordering};

/// Http Client Builder.
#[derive(Debug)]
pub struct HttpClientBuilder {
	max_request_body_size: u32,
}

impl HttpClientBuilder {
	/// Sets the maximum size of a request body in bytes (default is 10 MiB).
	pub fn max_request_body_size(mut self, size: u32) -> Self {
		self.max_request_body_size = size;
		self
	}

	/// Build the HTTP client with target to connect to.
	pub fn build(self, target: impl AsRef<str>) -> Result<HttpClient, Error> {
		let transport = HttpTransportClient::new(target, self.max_request_body_size)
			.map_err(|e| Error::TransportError(Box::new(e)))?;
		Ok(HttpClient { transport, request_id: AtomicU64::new(0) })
	}
}

impl Default for HttpClientBuilder {
	fn default() -> Self {
		Self { max_request_body_size: TEN_MB_SIZE_BYTES }
	}
}

/// JSON-RPC HTTP Client that provides functionality to perform method calls and notifications.
#[derive(Debug)]
pub struct HttpClient {
	/// HTTP transport client.
	transport: HttpTransportClient,
	/// Request ID that wraps around when overflowing.
	request_id: AtomicU64,
}

#[async_trait]
impl Client for HttpClient {
	async fn notification<'a>(&self, method: &'a str, params: JsonRpcParams<'a>) -> Result<(), Error> {
		let notif = JsonRpcNotificationSer::new(method, params);
		self.transport
			.send(serde_json::to_string(&notif).map_err(Error::ParseError)?)
			.await
			.map_err(|e| Error::TransportError(Box::new(e)))
	}

	/// Perform a request towards the server.
	async fn request<'a, R>(&self, method: &'a str, params: JsonRpcParams<'a>) -> Result<R, Error>
	where
		R: DeserializeOwned,
	{
		// NOTE: `fetch_add` wraps on overflow which is intended.
		let id = self.request_id.fetch_add(1, Ordering::Relaxed);
		let request = JsonRpcCallSer::new(Id::Number(id), method, params);

		let body = self
			.transport
			.send_and_read_body(serde_json::to_string(&request).map_err(Error::ParseError)?)
			.await
			.map_err(|e| Error::TransportError(Box::new(e)))?;

		let response: JsonRpcResponse<_> = match serde_json::from_slice(&body) {
			Ok(response) => response,
			Err(_) => {
				let err: JsonRpcErrorAlloc = serde_json::from_slice(&body).map_err(Error::ParseError)?;
				return Err(Error::Request(err));
			}
		};

		let response_id = parse_request_id(response.id)?;

		if response_id == id {
			Ok(response.result)
		} else {
			Err(Error::InvalidRequestId)
		}
	}

	async fn batch_request<'a, R>(&self, batch: Vec<(&'a str, JsonRpcParams<'a>)>) -> Result<Vec<R>, Error>
	where
		R: DeserializeOwned + Default + Clone,
	{
		let mut batch_request = Vec::with_capacity(batch.len());
		// NOTE(niklasad1): `ID` is not necessarily monotonically increasing.
		let mut ordered_requests = Vec::with_capacity(batch.len());
		let mut request_set = FnvHashMap::with_capacity_and_hasher(batch.len(), Default::default());

		for (pos, (method, params)) in batch.into_iter().enumerate() {
			let id = self.request_id.fetch_add(1, Ordering::SeqCst);
			batch_request.push(JsonRpcCallSer::new(Id::Number(id), method, params));
			ordered_requests.push(id);
			request_set.insert(id, pos);
		}

		let body = self
			.transport
			.send_and_read_body(serde_json::to_string(&batch_request).map_err(Error::ParseError)?)
			.await
			.map_err(|e| Error::TransportError(Box::new(e)))?;

		let rps: Vec<JsonRpcResponse<_>> = match serde_json::from_slice(&body) {
			Ok(response) => response,
			Err(_) => {
				let err: JsonRpcErrorAlloc = serde_json::from_slice(&body).map_err(Error::ParseError)?;
				return Err(Error::Request(err));
			}
		};

		// NOTE: `R::default` is placeholder and will be replaced in loop below.
		let mut responses = vec![R::default(); ordered_requests.len()];
		for rp in rps {
			let response_id = parse_request_id(rp.id)?;
			let pos = match request_set.get(&response_id) {
				Some(pos) => *pos,
				None => return Err(Error::InvalidRequestId),
			};
			responses[pos] = rp.result
		}
		Ok(responses)
	}
}

fn parse_request_id(raw: Option<&JsonRawValue>) -> Result<u64, Error> {
	match raw {
		None => Err(Error::InvalidRequestId),
		Some(id) => {
			let id = serde_json::from_str(id.get()).map_err(Error::ParseError)?;
			Ok(id)
		}
	}
}
