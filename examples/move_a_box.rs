use std::collections::VecDeque;
use std::error::Error;
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, SystemTime};

use bevy::prelude::*;
use bevy_inspector_egui::quick::WorldInspectorPlugin;
use clap::Parser;
use serde::{Deserialize, Serialize};

use bevy_replicon::prelude::*;
use bevy_replicon::renet::transport::{
    ClientAuthentication, NetcodeClientTransport, NetcodeServerTransport, ServerAuthentication,
    ServerConfig,
};
use bevy_replicon::renet::{ConnectionConfig, ServerEvent};
use log::info;

const MAX_TICK_RATE: u16 = 60;

fn main() {
    App::new()
        .init_resource::<Cli>() // Parse CLI before creating window.
        .add_plugins(DefaultPlugins.build().set(WindowPlugin {
            primary_window: Some(Window {
                title: "Move a Box".into(),
                resolution: (800.0, 600.0).into(),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .add_plugins(WorldInspectorPlugin::default())
        .add_plugins(
            (ReplicationPlugins
                .build()
                .set(ServerPlugin::new(TickPolicy::MaxTickRate(MAX_TICK_RATE)))),
        )
        .replicate::<PlayerServerPosition>()
        .replicate::<PlayerColor>()
        .add_client_event::<MoveCommandEvent>(SendPolicy::Ordered)
        .add_systems(
            Startup,
            (cli_system.pipe(system_adapter::unwrap), init_system),
        )
        // Systems that run only on the server or a single player instance
        .add_systems(Update, (movement_system).run_if(has_authority()))
        // Systems that run only on the server
        .add_systems(
            Update,
            (server_event_system).run_if(resource_exists::<RenetServer>()),
        )
        // Systems that run only on the client or a single player instance
        .add_systems(Update, (input_system).run_if(is_client_or_single_player()))
        // Systems that run everywhere
        .add_systems(Update, draw_box_system)
        // Snapshot Interpolation
        .add_systems(
            Update,
            (
                player_init_system,
                snapshot_buffer_system,
                interpolation_system,
            )
                .run_if(resource_exists::<RenetClient>()),
        )
        .run();
}

#[derive(Component, Deserialize, Serialize)]
struct PlayerServerPosition(Vec2);

#[derive(Component)]
struct PlayerPositionSnapshotBuffer(VecDeque<Vec2>);

#[derive(Component)]
struct PlayerClientPosition(Vec2);

#[derive(Component, Deserialize, Serialize)]
struct PlayerColor(Color);

/// Contains the client ID of the player
#[derive(Component, Serialize, Deserialize)]
struct Player(u64);

#[derive(Bundle)]
struct PlayerBundle {
    player: Player,
    server_position: PlayerServerPosition,
    client_position: PlayerClientPosition,
    color: PlayerColor,
    replication: Replication,
}

impl PlayerBundle {
    fn new(id: u64, position: Vec2, color: Color) -> Self {
        Self {
            player: Player(id),
            server_position: PlayerServerPosition(position),
            client_position: PlayerClientPosition(position),
            color: PlayerColor(color),
            replication: Replication,
        }
    }
}

fn player_init_system(
    mut commands: Commands,
    spawned_players: Query<(Entity, &PlayerServerPosition), Added<PlayerServerPosition>>,
) {
    for (entity, server_pos) in &spawned_players {
        commands.entity(entity).insert((
            PlayerClientPosition(server_pos.0),
            PlayerPositionSnapshotBuffer(vec![server_pos.0].into()),
        ));
    }
}

#[derive(Resource)]
struct ServerTickTimer(Timer);

fn init_system(mut commands: Commands) {
    commands.spawn(Camera2dBundle::default());
    commands.insert_resource(ServerTickTimer(Timer::new(
        Duration::from_secs_f32(1.0 / MAX_TICK_RATE as f32),
        TimerMode::Once,
    )))
}

fn draw_box_system(q_net_pos: Query<(&PlayerClientPosition, &PlayerColor)>, mut gizmos: Gizmos) {
    for (p, color) in q_net_pos.iter() {
        gizmos.rect(
            Vec3::new(p.0.x, p.0.y, 0.),
            Quat::IDENTITY,
            Vec2::ONE * 50.,
            color.0,
        );
    }
}

#[derive(Debug, Default, Deserialize, Event, Serialize)]
struct MoveCommandEvent {
    pub direction: Vec2,
}

// Read player inputs and publish MoveCommandEvents, only for client or single player instance
fn input_system(input: Res<Input<KeyCode>>, mut move_event: EventWriter<MoveCommandEvent>) {
    let right = input.pressed(KeyCode::Right);
    let left = input.pressed(KeyCode::Left);
    let up = input.pressed(KeyCode::Up);
    let down = input.pressed(KeyCode::Down);
    let mut direction = Vec2::ZERO;
    if right {
        direction.x += 1.0;
    }
    if left {
        direction.x -= 1.0;
    }
    if up {
        direction.y += 1.0;
    }
    if down {
        direction.y -= 1.0;
    }
    if right || left || up || down {
        move_event.send(MoveCommandEvent {
            direction: direction.normalize_or_zero(),
        })
    }
}

// Mutate PlayerPosition based on MoveCommandEvents, runs on server or single player instance
fn movement_system(
    mut events: EventReader<FromClient<MoveCommandEvent>>,
    mut q_net_pos: Query<(
        &Player,
        &mut PlayerServerPosition,
        &mut PlayerClientPosition,
    )>,
    time: Res<Time>,
) {
    let move_speed = 300.0;
    for FromClient { client_id, event } in &mut events {
        // info!("received event {event:?} from client {client_id}");
        for (player, mut position, mut client_position) in q_net_pos.iter_mut() {
            if *client_id == player.0 {
                position.0 += event.direction * time.delta_seconds() * move_speed;
                client_position.0 = position.0;
            }
        }
    }
}

fn snapshot_buffer_system(
    mut q_player: Query<
        (&PlayerServerPosition, &mut PlayerPositionSnapshotBuffer),
        Changed<PlayerServerPosition>,
    >,
    mut server_tick_timer: ResMut<ServerTickTimer>,
) {
    for (server_pos, mut snapshot_buffer) in q_player.iter_mut() {
        snapshot_buffer.0.push_back(server_pos.0);
        if snapshot_buffer.0.len() > 2 {
            snapshot_buffer.0.pop_front();
        }
        server_tick_timer.0.reset();
    }
}

fn interpolation_system(
    mut q_player: Query<(&PlayerPositionSnapshotBuffer, &mut PlayerClientPosition)>,
    mut server_tick_timer: ResMut<ServerTickTimer>,
    time: Res<Time>,
) {
    for (snapshot_buffer, mut client_pos) in q_player.iter_mut() {
        if snapshot_buffer.0.len() < 2 {
            continue;
        }

        let tick_duration = 1.0 / (MAX_TICK_RATE as f32);

        let t = if server_tick_timer.0.finished() {
            1.0
        } else {
            server_tick_timer.0.elapsed_secs() / tick_duration
        };
        client_pos.0 = snapshot_buffer.0[0].lerp(snapshot_buffer.0[1], t);
    }
    server_tick_timer
        .0
        .tick(Duration::from_secs_f32(time.delta_seconds()));
}

// Spawns a new player whenever a client connects
fn server_event_system(mut commands: Commands, mut server_event: EventReader<ServerEvent>) {
    for event in &mut server_event {
        match event {
            ServerEvent::ClientConnected { client_id } => {
                info!("Player: {client_id} Connected");
                // Generate pseudo random color from client id
                let r = ((client_id % 23) as f32) / 23.;
                let g = ((client_id % 27) as f32) / 27.;
                let b = ((client_id % 39) as f32) / 39.;
                commands.spawn(PlayerBundle::new(
                    *client_id,
                    Vec2::ZERO,
                    Color::rgb(r, g, b),
                ));
            }
            ServerEvent::ClientDisconnected { client_id, reason } => {
                info!("Client {client_id} disconnected: {reason}");
            }
        }
    }
}

const PORT: u16 = 5000;
const PROTOCOL_ID: u64 = 0;

#[derive(Debug, Parser, PartialEq, Resource)]
enum Cli {
    SinglePlayer,
    Server {
        #[arg(short, long, default_value_t = PORT)]
        port: u16,
    },
    Client {
        #[arg(short, long, default_value_t = Ipv4Addr::LOCALHOST.into())]
        ip: IpAddr,

        #[arg(short, long, default_value_t = PORT)]
        port: u16,
    },
}

impl Default for Cli {
    fn default() -> Self {
        Self::parse()
    }
}

fn cli_system(
    mut commands: Commands,
    cli: Res<Cli>,
    network_channels: Res<NetworkChannels>,
) -> Result<(), Box<dyn Error>> {
    match *cli {
        Cli::SinglePlayer => {
            commands.spawn(PlayerBundle::new(0, Vec2::ZERO, Color::GREEN));
        }
        Cli::Server { port } => {
            let server_channels_config = network_channels.server_channels();
            let client_channels_config = network_channels.client_channels();

            let server = RenetServer::new(ConnectionConfig {
                server_channels_config,
                client_channels_config,
                ..Default::default()
            });

            let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
            let public_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
            let socket = UdpSocket::bind(public_addr)?;
            let server_config = ServerConfig {
                max_clients: 10,
                protocol_id: PROTOCOL_ID,
                public_addr,
                authentication: ServerAuthentication::Unsecure,
            };
            let transport = NetcodeServerTransport::new(current_time, server_config, socket)?;

            commands.insert_resource(server);
            commands.insert_resource(transport);

            commands.spawn(TextBundle::from_section(
                "Server",
                TextStyle {
                    font_size: 30.0,
                    color: Color::WHITE,
                    ..default()
                },
            ));
        }
        Cli::Client { port, ip } => {
            let server_channels_config = network_channels.server_channels();
            let client_channels_config = network_channels.client_channels();

            let client = RenetClient::new(ConnectionConfig {
                server_channels_config,
                client_channels_config,
                ..Default::default()
            });

            let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
            let client_id = current_time.as_millis() as u64;
            let server_addr = SocketAddr::new(ip, port);
            let socket = UdpSocket::bind((ip, 0))?;
            let authentication = ClientAuthentication::Unsecure {
                client_id,
                protocol_id: PROTOCOL_ID,
                server_addr,
                user_data: None,
            };
            let transport = NetcodeClientTransport::new(current_time, authentication, socket)?;

            commands.insert_resource(client);
            commands.insert_resource(transport);

            commands.spawn(TextBundle::from_section(
                format!("Client: {client_id:?}"),
                TextStyle {
                    font_size: 30.0,
                    color: Color::WHITE,
                    ..default()
                },
            ));
        }
    }

    Ok(())
}

fn is_client_or_single_player(
) -> impl FnMut(Option<Res<RenetClient>>, Option<Res<RenetServer>>) -> bool + Clone {
    move |client, server| client.is_some() || server.is_none()
}

// Interpolation

#[derive(Component, Deserialize, Serialize)]
struct Interpolated;

trait Interpolate {
    fn interpolate(&self, other: Self, t: f32) -> Self;
}

#[derive(Component, Deserialize, Serialize)]
struct SnapshotBuffer<T>(VecDeque<T>);
fn test<T: Component + Interpolate>(q: Query<(Entity, &mut T)>) {}
