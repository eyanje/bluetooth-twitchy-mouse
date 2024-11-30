use bluer::{Address, Uuid};
use btmgmt::Client;
use btmgmt::command::{SetDeviceId};
use btmgmt::packet::DeviceIdSource;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Read};
use std::pin::{pin};
use std::string::String;
use libc::{ETIMEDOUT};
use tokio::{select, spawn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast};
use tokio::time;

use hid_device_class::{MajorServiceClass, make_class_of_device, MinorDeviceClass, Peripheral, PeripheralLower, PeripheralUpper};
use hid_device_configuration::{Configuration, encoding, hid, language};
use hid_device_report::collection::Collection;
use hid_device_report::format::ReportFormat;
use hid_device_report::iter::ToReportIterator;
use hid_device_report::report::Report;
use hid_device_report::field_types::{CollectionType, ReportFlags};
use hid_device_report::usage::UsageSet;
use hid_device_report::usage_tables;
use hidp;
use hid_device_id::usb;

mod connection;

use connection::{Connection, ConnectionControl, new_control_listener, new_interrupt_listener};

#[derive(Debug)]
enum ErrorAdapter {
    String(String),
    BlueR(bluer::Error),
    Io(io::Error),
}

macro_rules! impl_from_trivial {
    ($t:ty => $ea:ident) => {
        /// Convert some error into an ErrorAdapter
        impl From<$t> for ErrorAdapter {
            fn from(value: $t) -> Self {
                Self::$ea(value)
            }
        }
    };
}

impl_from_trivial!(String => String);
impl_from_trivial!(bluer::Error => BlueR);
impl_from_trivial!(io::Error => Io);

impl Display for ErrorAdapter {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            ErrorAdapter::String(s) => s.fmt(f),
            ErrorAdapter::BlueR(e) => e.fmt(f),
            ErrorAdapter::Io(e) => e.fmt(f),
        }
    }
}

enum ReportProtocol {
    Boot,
    Report,
}

/// Handle a single connection by twitching the mouse.
///
/// This function switches protocols automatically on request.
async fn handle_connection(
    mut connection: Connection,
    report_format: ReportFormat,
    boot_report_format: ReportFormat) {

    const BUF_SIZE: usize = 1024;
    const TIMER: time::Duration = time::Duration::from_millis(100);
    let mut control_buf = [0u8; BUF_SIZE];
    let mut interrupt_buf = [0u8; BUF_SIZE];
    let mut frame = 0u8;
    let mut timer = pin!(time::sleep(TIMER));
    let mut mode = ReportProtocol::Report;

    let mouse_button = [0i32; 3];
    let mut mouse_rel = [0i32; 2];

    loop {
        select! {
            // Wait for the timer to expire
            _ = &mut timer => {
                // Restart the timer
                timer.set(time::sleep(TIMER));
                // Advance the frame count by one
                (frame, _) = frame.overflowing_add(1);

                mouse_rel[0] = 10 - 20 * i32::from(frame % 2);
    
                // Fill out a report
                let filled_report = match mode {
                    ReportProtocol::Boot => {
                        // All boot protocol mods reports must have an ID attached. This ID is
                        // declared in section 3.3.2 of the Bluetooth HID protocol.
                        let mut fillable_report_format = boot_report_format.clone();
                        fillable_report_format[0].set_signed(mouse_button[0]).unwrap();
                        fillable_report_format[1].set_signed(mouse_button[1]).unwrap();
                        fillable_report_format[2].set_signed(mouse_button[2]).unwrap();
                        fillable_report_format[3].set_signed(mouse_rel[0]).unwrap();
                        fillable_report_format[4].set_signed(mouse_rel[1]).unwrap();
                        fillable_report_format
                    },
                    ReportProtocol::Report => {
                        let mut fillable_report_format = report_format.clone();
                        fillable_report_format[0].set_signed(mouse_button[0]).unwrap();
                        fillable_report_format[1].set_signed(mouse_button[1]).unwrap();
                        fillable_report_format[2].set_signed(mouse_button[2]).unwrap();
                        fillable_report_format[3].set_signed(mouse_rel[0]).unwrap();
                        fillable_report_format[4].set_signed(mouse_rel[1]).unwrap();
                        fillable_report_format
                    },
                };
                // Reset relative axes.
                mouse_rel.fill(0);
        
                if let Some(interrupt_socket) = connection.interrupt_socket.inner_mut() {
                    // Send the report now and wait
                    let message = hidp::Message::new_data_input(filled_report.into_bytes());
                    AsyncWriteExt::write(
                        interrupt_socket,
                        &message.as_bytes()).await.unwrap();
                }
            },
            // Read from the control socket.
            control_len = AsyncReadExt::read(
                &mut connection.control_socket,
                &mut control_buf) => {
                match control_len {
                    Ok(len) => {
                        // Parse control buffer data as a Message.
                        match hidp::Message::read_from(&control_buf[..len]) {
                            Ok(message) => {
                                // Do stuff
                                println!("Received message {:?}", message);
                                match message {
                                    hidp::Message::Handshake(_) => (),
                                    hidp::Message::HidControl(_) => (),
                                    hidp::Message::GetReport(_parameter, _data) => {
                                        // Handle stuff, then send back data.
                                    },
                                    hidp::Message::SetReport(_parameter, _data) => {
                                    },
                                    hidp::Message::GetProtocol(_parameter) => {
                                    },
                                    hidp::Message::SetProtocol(parameter) => {
                                        let reply = match parameter {
                                            hidp::protocol::BOOT => {
                                                mode = ReportProtocol::Boot;
                                                hidp::Message::Handshake(
                                                    hidp::handshake::SUCCESSFUL)
                                            },
                                            hidp::protocol::REPORT => {
                                                mode = ReportProtocol::Report;
                                                hidp::Message::Handshake(
                                                    hidp::handshake::SUCCESSFUL)
                                            },
                                            _ => {
                                                hidp::Message::Handshake(
                                                    hidp::handshake::ERR_INVALID_PARAMETER)
                                            },
                                        };

                                        AsyncWriteExt::write(
                                            &mut connection.control_socket,
                                            &reply.as_bytes()).await.unwrap();
                                    },
                                    hidp::Message::Data(..) => (),
                                }
                                // TODO: respond with HANDSHAKE
                            },
                            Err(e) => {
                                eprintln!("Error parsing data from control {}", e);
                                // Data is likely malformed.
                                // Perhaps break, perhaps ignore?
                            },
                        }
                    },
                    // If reading gives an error, stop the loop.
                    Err(e) => {
                        println!("Received error from control {:?}", e);
                        break;
                    }
                }
            },
            // Read data from the interrupt socket.
            len = AsyncReadExt::read(
                &mut connection.interrupt_socket,
                &mut interrupt_buf) => {
                match len {
                    Ok(_len) => {
                        // Ignore interrupt out messages
                    },
                    // If reading gives an error, stop the loop.
                    Err(e) => {
                        println!("Received error from interrupt {:?}", e);
                        break;
                    }
                }
            },
            // Receive events from the main loop
            event = connection.event_rx.recv() => {
                match event {
                    Some(connection::Event::InterruptEstablished(interrupt_socket)) => {
                        connection.interrupt_socket.replace(interrupt_socket);
                    },
                    // Channel has closed
                    None => {
                        println!("Event channel closed. Terminating connection with {}", connection.peer_address());
                        break;
                    }
                }
            },
        }
    }
}


#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ConnectionData {
    sent_interrupt: bool,
}


#[tokio::main]
async fn main() -> Result<(), ErrorAdapter> {
    let report_descriptor = Collection::new(
        CollectionType::Application,
        usage_tables::generic_desktop::MOUSE, [
        Collection::new(
            CollectionType::Physical,
            usage_tables::generic_desktop::POINTER,
            [
            Report::new_input(
                ReportFlags::new().as_data().as_variable().as_absolute(),
                UsageSet::empty()
                    .with_usage_bounds(
                        usage_tables::button::new(0x01),
                        usage_tables::button::new(0x03)),
                // Logical extents
                0, 1,
                // Size
                1,
                // Count
                3).with_report_id(2),
            Report::new_input(
                ReportFlags::new().as_constant(),
                UsageSet::empty(),
                // Logical extents
                0, 1,
                // Size
                5,
                // Count
                1).with_report_id(2),
            Report::new_input(
                ReportFlags::new().as_data().as_variable().as_relative(),
                UsageSet::empty()
                    .with_usage(usage_tables::generic_desktop::X)
                    .with_usage(usage_tables::generic_desktop::Y),
                // Logical extents
                -127, 127,
                // Size
                8,
                // Count
                2).with_report_id(2),
        ])]);

    // Construct a report format for a report, using id 2 (mouse).
    let report_format = report_descriptor.input_report_format(Some(2)).unwrap();

    // Create a second configuration from the shorter syntax.
    let device_subclass = Peripheral::new(PeripheralUpper::PointingDevice,
                                          PeripheralLower::Uncategorized);
    let version = 0x0101;
    let config = Configuration {
        primary_language: language::ENGLISH,
        encoding: encoding::UTF_8,

        service_name: Some("QTLHN Wireless Mouse".to_string()),
        service_description: Some("Bluetooth pointing device".to_string()),
        provider_name: Some("QTLHN".to_string()),

        version,

        hid: hid::Configuration {
            device_subclass: device_subclass.minor_device_class().try_into().unwrap(),
            boot_device: true,
            virtual_cable: true,
            reconnect_initiate: true,

            country_code: usb::country_code::US,
            class_descriptors: vec!(hid::ClassDescriptor::report(report_descriptor.into_bytes().to_vec())),
            ..hid::Configuration::default()
        }
    };

    let document = &config.to_sdp_tag();

    // Connect to BlueZ
    
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    // Register profile
    let profile_uuid = Uuid::from(bluer::id::ServiceClass::Hid);
    println!("Registering profile: {}", profile_uuid);
    let _profile_handle = session.register_profile(bluer::rfcomm::Profile {
        service_record: Some(document.to_xml_document()),
        version: Some(version),

        ..bluer::rfcomm::Profile::default()
    }).await?;
    

    // Enable discoverability
    // Possibly not needed, since we're running something like a server.
    println!("Enabling discoverability");
    adapter.set_discoverable(true).await?;

    // Change device ID.
    // Need to use btmgmt because there is no equivalent HCI command.
    println!("Setting device ID");
    let client = Client::open().unwrap();
    let device_id_result = client.call(Some(0), SetDeviceId {
        source: DeviceIdSource::UsbImplementersForum,
        vendor: 0x057e,
        product: 0x0306,
        version: 0x0600,
    }).await;
    if let Err(e) = device_id_result {
        eprintln!("While setting device ID: {}", e);
    }
    
    // Change class.
    println!("Connecting to HCI");
    let device_id = 0;
    let mut hci_client = bluez_hci::Socket::new(device_id)?;

    // Write class but don't wait for a response.
    println!("Writing class");
    let class_of_device = make_class_of_device(MajorServiceClass::empty(), device_subclass);
    match hci_client.write_class_of_dev(class_of_device, 2000) {
        Ok(_) => (),
        Err(e) if e.raw_os_error() == Some(ETIMEDOUT) => {
            eprintln!("Timed out while writing class");
        },
        Err(e) => {
            eprintln!("Error while setting class: {}", e);
        },
    }

    println!("Establishing listeners");

    let control_listener = new_control_listener(Address::any())?;
    println!("Listening for control connection");

    let interrupt_listener = new_interrupt_listener(Address::any())?;
    println!("Listening for interrupt connection");

    // Wait for connections

    let mut connections: Vec<ConnectionControl<ConnectionData>> = Vec::new();

    // Spawn a task to read from control.
    let (user_input_tx, mut user_input_rx) = broadcast::channel::<()>(16);
    spawn(async move {
        let _tx = user_input_tx; // move
        let mut stdin = std::io::stdin();
        // Rather than quit externally, we should put REPL code in this thread and send the
        // termination signal out.
        loop {
            let mut b = [0u8];
            match stdin.read(&mut b) {
                Ok(_) => {
                    // Terminate the program.
                    break;
                },
                Err(_) => {
                    // Also terminate the program.
                    break;
                },
            }
        }
        println!("Exited loop");
    });
        

    loop {
        select! {
            control_result = control_listener.accept() => {
                match control_result {
                    Ok((control, peer_addr)) => {
                        // Clear dead connections
                        connections.retain(|c| !c.event_tx.is_closed());
                        
                        // Find existing (live) connections.
                        let connection_exists = connections.iter()
                            .any(|c| c.peer_address == peer_addr.addr);

                        if connection_exists {
                            eprintln!("Duplicate control connection to {}", peer_addr.addr);
                        } else {
                            // Create a new connection with the peer
                            let new_connection = Connection::new(peer_addr.addr, control);
                            // Create a controller, which we can keep to send events to the connection handler.
                            let new_controller = new_connection.new_controller(ConnectionData::default());
                            connections.push(new_controller);
            
                            println!("Accepted control connection from {}", peer_addr.addr);
            
                            // Move the connection to a separate handler
                            let report_format_clone = report_format.clone();
                            let boot_report_format_clone = report_format.clone();
                            spawn(async move {
                                handle_connection(
                                    new_connection,
                                    report_format_clone,
                                    boot_report_format_clone).await;
                            });
                        }
                    },
                    // If we could not receive the control connection, exit.
                    Err(e) => {
                        eprintln!("Error receiving control connection: {}", e);
                        break;
                    }
                }
            },
            interrupt_result = interrupt_listener.accept() => {
                match interrupt_result {
                    Ok((interrupt, peer_addr)) => {
                        let existing_connection_opt = connections.iter()
                            .find(|c| c.peer_address == peer_addr.addr);
                        if let Some(existing_connection) = existing_connection_opt {
                            if existing_connection.data.sent_interrupt {
                                eprintln!("Duplicate interrupt connection to {}", peer_addr.addr);
                            } else {
                                // Send an interrupt established event.
                                // Ignore errors, such as if there is not receiver.
                                let _ = existing_connection.event_tx
                                    .send(connection::Event::InterruptEstablished(interrupt)).await;
                                println!("Accepted interrupt connection from {}", peer_addr.addr);
                            }
                        } else {
                            eprintln!("Dangling interrupt connection received from {}", peer_addr.addr);
                        }
                    },
                    // If we could not receive the interrupt connection, exit.
                    Err(e) => {
                        eprintln!("Error receiving interrupt connection: {}", e);
                        break;
                    }
                }
            },
            user_input = user_input_rx.recv() => {
                match user_input {
                    Ok(_) => (), // No events currently.
                    Err(_) => {
                        // Broadcast has closed. Exiting...
                        break;
                    },
                }
            },
        }
    }

    println!("Unregistering profile: {}", profile_uuid);
    
    Ok(())
}
