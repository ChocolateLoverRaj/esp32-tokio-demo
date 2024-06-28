use std::convert::Infallible;
use esp_idf_svc::hal::prelude::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::timer::EspTaskTimerService;
use esp_idf_svc::wifi::{AsyncWifi, EspWifi, AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::sys::{esp, EspError};
use http_body_util::{BodyExt, Empty, Full};
use http_body_util::combinators::BoxBody;
use hyper::{Method, Request, Response, StatusCode};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_tungstenite::HyperWebsocket;
use hyper_tungstenite::tungstenite::Message;
use hyper_util::rt::TokioIo;
use log::info;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use futures::sink::SinkExt;
use futures::stream::StreamExt;

// Edit these or provide your own way of provisioning...
const WIFI_SSID: &str = "AJR2";
const WIFI_PASS: &str = "Rnr12345";

// To test, run `cargo run`, then when the server is up, use `nc -v espressif 12345` from
// a machine on the same Wi-Fi network.
const TCP_LISTENING_PORT: u16 = 80;

fn main() -> anyhow::Result<()> {
  // It is necessary to call this function once. Otherwise, some patches to the runtime
  // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
  esp_idf_svc::sys::link_patches();
  esp_idf_svc::log::EspLogger::initialize_default();

  // eventfd is needed by our mio poll implementation.  Note you should set max_fds
  // higher if you have other code that may need eventfd.
  info!("Setting up eventfd...");
  let config = esp_idf_svc::sys::esp_vfs_eventfd_config_t {
    max_fds: 5,
    ..Default::default()
  };
  esp! { unsafe { esp_idf_svc::sys::esp_vfs_eventfd_register(&config) } }?;

  info!("Setting up board...");
  let peripherals = Peripherals::take().unwrap();
  let sysloop = EspSystemEventLoop::take()?;
  let timer = EspTaskTimerService::new()?;
  let nvs = EspDefaultNvsPartition::take()?;

  info!("Initializing Wi-Fi...");
  let wifi = AsyncWifi::wrap(
    EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
    sysloop,
    timer.clone())?;

  info!("Starting async run loop");
  tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()?
      .block_on(async move {
        let mut wifi_loop = WifiLoop { wifi };
        wifi_loop.configure().await?;
        wifi_loop.initial_connect().await?;

        info!("Preparing to launch echo server...");
        tokio::spawn(echo_server());

        info!("Entering main Wi-Fi run loop...");
        wifi_loop.stay_connected().await
      })?;

  Ok(())
}

pub struct WifiLoop<'a> {
  wifi: AsyncWifi<EspWifi<'a>>,
}

impl<'a> WifiLoop<'a> {
  pub async fn configure(&mut self) -> Result<(), EspError> {
    info!("Setting Wi-Fi credentials...");
    let wifi_configuration: Configuration = Configuration::Client(ClientConfiguration {
      ssid: WIFI_SSID.parse().unwrap(),
      password: WIFI_PASS.parse().unwrap(),
      auth_method: AuthMethod::WPA2Personal,
      channel: None,
      ..Default::default()
    });
    self.wifi.set_configuration(&wifi_configuration)?;

    info!("Starting Wi-Fi driver...");
    self.wifi.start().await
  }

  pub async fn initial_connect(&mut self) -> Result<(), EspError> {
    self.do_connect_loop(true).await
  }

  pub async fn stay_connected(mut self) -> Result<(), EspError> {
    self.do_connect_loop(false).await
  }

  async fn do_connect_loop(
    &mut self,
    exit_after_first_connect: bool,
  ) -> Result<(), EspError> {
    let wifi = &mut self.wifi;
    loop {
      // Wait for disconnect before trying to connect again.  This loop ensures
      // we stay connected and is commonly missing from trivial examples as it's
      // way too difficult to showcase the core logic of an example and have
      // a proper Wi-Fi event loop without a robust async runtime.  Fortunately, we can do it
      // now!
      wifi.wifi_wait(|this| this.is_up(), None).await?;

      info!("Connecting to Wi-Fi...");
      wifi.connect().await?;

      info!("Waiting for association...");
      wifi.ip_wait_while(|this| this.is_up().map(|s| !s), None).await?;

      if exit_after_first_connect {
        return Ok(());
      }
    }
  }
}

async fn echo_server() -> anyhow::Result<()> {
  let addr = format!("0.0.0.0:{TCP_LISTENING_PORT}");

  info!("Binding to {addr}...");
  let listener = TcpListener::bind(&addr).await?;

  loop {
    info!("Waiting for new connection on socket: {listener:?}");
    let (stream, _) = listener.accept().await?;

    let io = TokioIo::new(stream);

    tokio::spawn(async move {
      info!("Spawned handler!");
      if let Err(err) = hyper::server::conn::http1::Builder::new()
          .keep_alive(true)
          // `service_fn` converts our function in a `Service`
          .serve_connection(io, service_fn(handle_request))
          .with_upgrades()
          .await
      {
        eprintln!("Error serving connection: {:?}", err);
      }
    });
  }
}

async fn handle_request(mut req: Request<hyper::body::Incoming>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Error> {
  // We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.
  fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
  }
  fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
  }

  // Check if the request is a websocket upgrade request.
  if hyper_tungstenite::is_upgrade_request(&req) {
    let (response, websocket) = hyper_tungstenite::upgrade(&mut req, None)?;

    // Spawn a task to handle the websocket connection.
    tokio::spawn(async move {
      if let Err(e) = serve_websocket(websocket).await {
        eprintln!("Error in websocket connection: {e}");
      }
    });

    // Return the response so the spawned future can continue.
    Ok(response.map(|a| a.map_err(|never| match never {}).boxed()))
  } else {
    // Handle regular HTTP requests here.
    match (req.method(), req.uri().path()) {
      (&Method::GET, uri) => {
        match uri {
          "/" => Ok(Response::new(full("This will be where index.html is served"))),
          _ => {
            let mut not_found = Response::new(empty());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
          }
        }
      },
      // Return 404 Not Found for other routes.
      _ => {
        let mut not_found = Response::new(empty());
        *not_found.status_mut() = StatusCode::NOT_FOUND;
        Ok(not_found)
      }
    }
  }
}


type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Handle a websocket connection.
async fn serve_websocket(websocket: HyperWebsocket) -> Result<(), Error> {
  let mut websocket = websocket.await?;
  while let Some(message) = websocket.next().await {
    match message? {
      Message::Text(msg) => {
        println!("Received text message: {msg}");
        websocket.send(Message::text("Thank you, come again.")).await?;
      },
      Message::Binary(msg) => {
        println!("Received binary message: {msg:02X?}");
        websocket.send(Message::binary(b"Thank you, come again.".to_vec())).await?;
      },
      Message::Ping(msg) => {
        // No need to send a reply: tungstenite takes care of this for you.
        println!("Received ping message: {msg:02X?}");
      },
      Message::Pong(msg) => {
        println!("Received pong message: {msg:02X?}");
      }
      Message::Close(msg) => {
        // No need to send a reply: tungstenite takes care of this for you.
        if let Some(msg) = &msg {
          println!("Received close message with code {} and message: {}", msg.code, msg.reason);
        } else {
          println!("Received close message");
        }
      },
      Message::Frame(_msg) => {
        unreachable!();
      }
    }
  }

  Ok(())
}
