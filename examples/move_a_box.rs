use std::collections::VecDeque;
use std::error::Error;
use std::fmt::Debug;
use std::io::Cursor;
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::SystemTime;

use bevy::ecs::world::EntityMut;
use bevy::prelude::*;
use bevy::ptr::Ptr;
use bevy_inspector_egui::quick::WorldInspectorPlugin;
use clap::Parser;
use log::info;
use serde::{Deserialize, Serialize};

use bevy_replicon::prelude::*;
use bevy_replicon::renet::transport::{
    ClientAuthentication, NetcodeClientTransport, NetcodeServerTransport, ServerAuthentication,
    ServerConfig,
};
use bevy_replicon::renet::{Bytes, ConnectionConfig, ServerEvent};
use bevy_replicon::replicon_core::replication_rules;
use bevy_replicon::replicon_core::replication_rules::{
    DeserializeFn, RemoveComponentFn, SerializeFn,
};

const MAX_TICK_RATE: u16 = 30;

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
        .add_plugins(SnapshotInterpolationPlugin {
            max_tick_rate: MAX_TICK_RATE,
        })
        .replicate_with_interpolation::<PlayerPosition>(
            serialize_player_position,
            deserialize_player_position,
            replication_rules::remove_component::<PlayerPosition>,
        )
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
        .run();
}

#[derive(Component, Deserialize, Serialize, Clone, Debug)]
struct PlayerPosition(Vec2);

impl Interpolate for PlayerPosition {
    fn interpolate(&self, other: Self, t: f32) -> Self {
        Self(self.0.lerp(other.0, t))
    }
}

pub fn deserialize_player_position(
    entity: &mut EntityMut,
    _entity_map: &mut ServerEntityMap,
    mut cursor: &mut Cursor<Bytes>,
    tick: RepliconTick,
) -> Result<(), bincode::Error> {
    let value: Vec2 = bincode::deserialize_from(&mut cursor)?;
    let component = PlayerPosition(value);
    if let Some(mut buffer) = entity.get_mut::<SnapshotBuffer<PlayerPosition>>() {
        buffer.insert(component);
    } else {
        entity.insert(component);
    }
    Ok(())
}

pub fn serialize_player_position(
    component: Ptr,
    mut cursor: &mut Cursor<Vec<u8>>,
) -> Result<(), bincode::Error> {
    // SAFETY: Function called for registered `ComponentId`.
    let component: &PlayerPosition = unsafe { component.deref() };
    bincode::serialize_into(cursor, &component)
}

#[derive(Component, Deserialize, Serialize)]
struct PlayerColor(Color);

/// Contains the client ID of the player
#[derive(Component, Serialize, Deserialize)]
struct Player(u64);

#[derive(Bundle)]
struct PlayerBundle {
    player: Player,
    server_position: PlayerPosition,
    color: PlayerColor,
    replication: Replication,
    interpolated: Interpolated,
}

impl PlayerBundle {
    fn new(id: u64, position: Vec2, color: Color) -> Self {
        Self {
            player: Player(id),
            server_position: PlayerPosition(position),
            color: PlayerColor(color),
            replication: Replication,
            interpolated: Interpolated,
        }
    }
}

fn init_system(mut commands: Commands) {
    commands.spawn(Camera2dBundle::default());
}

fn draw_box_system(q_net_pos: Query<(&PlayerPosition, &PlayerColor)>, mut gizmos: Gizmos) {
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
    mut q_net_pos: Query<(&Player, &mut PlayerPosition)>,
    time: Res<Time>,
) {
    let move_speed = 300.0;
    for FromClient { client_id, event } in &mut events {
        // info!("received event {event:?} from client {client_id}");
        for (player, mut position) in q_net_pos.iter_mut() {
            if *client_id == player.0 {
                position.0 += event.direction * time.delta_seconds() * move_speed;
            }
        }
    }
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

struct SnapshotInterpolationPlugin {
    max_tick_rate: u16,
}
impl Plugin for SnapshotInterpolationPlugin {
    fn build(&self, app: &mut App) {
        app.replicate::<Interpolated>()
            .insert_resource(InterpolationConfig {
                max_tick_rate: self.max_tick_rate,
            });
    }
}

#[derive(Resource)]
struct InterpolationConfig {
    max_tick_rate: u16,
}

#[derive(Component, Deserialize, Serialize)]
struct Interpolated;

trait Interpolate {
    fn interpolate(&self, other: Self, t: f32) -> Self;
}

#[derive(Component, Deserialize, Serialize)]
struct SnapshotBuffer<T: Component + Interpolate + Clone> {
    buffer: VecDeque<T>,
    time_since_last_snapshot: f32,
}
impl<T: Component + Interpolate + Clone> SnapshotBuffer<T> {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            time_since_last_snapshot: 0.0,
        }
    }
    pub fn insert(&mut self, element: T) {
        if self.buffer.len() > 1 {
            self.buffer.pop_front();
        }
        self.buffer.push_back(element);
        self.time_since_last_snapshot = 0.0;
    }
}

fn snapshot_buffer_init_system<T: Component + Interpolate + Clone>(
    q_interpolated: Query<(Entity, &T), Added<Interpolated>>,
    mut commands: Commands,
) {
    for (e, initial_value) in q_interpolated.iter() {
        let mut buffer = SnapshotBuffer::new();
        buffer.insert(initial_value.clone());
        commands.entity(e).insert(buffer);
        info!("intialized buffer");
    }
}
fn snapshot_interpolation_system<T: Component + Interpolate + Clone + Debug>(
    mut q: Query<(Entity, &mut T, &mut SnapshotBuffer<T>), With<Interpolated>>,
    time: Res<Time>,
    config: Res<InterpolationConfig>,
) {
    for (e, mut component, mut snapshot_buffer) in q.iter_mut() {
        let buffer = &snapshot_buffer.buffer;
        let elapsed = snapshot_buffer.time_since_last_snapshot;
        if buffer.len() < 2 {
            continue;
        }

        let tick_duration = 1.0 / (config.max_tick_rate as f32);

        if elapsed > tick_duration + time.delta_seconds() {
            continue;
        }

        let t = (elapsed / tick_duration).clamp(0., 1.);
        info!(
            "Snapshot 0: {:?} ,Snapshot 1: {:?}, t: {:?}",
            buffer[0].clone(),
            buffer[1].clone(),
            t
        );
        *component = buffer[0].interpolate(buffer[1].clone(), t);
        snapshot_buffer.time_since_last_snapshot += time.delta_seconds();
    }
}

trait AppInterpolationExt {
    /// TODO: Add docs
    fn replicate_with_interpolation<C>(
        &mut self,
        serialize: SerializeFn,
        deserialize: DeserializeFn,
        remove: RemoveComponentFn,
    ) -> &mut Self
    where
        C: Component + Interpolate + Clone + Debug;
}
impl AppInterpolationExt for App {
    fn replicate_with_interpolation<T>(
        &mut self,
        serialize: SerializeFn,
        deserialize: DeserializeFn,
        remove: RemoveComponentFn,
    ) -> &mut Self
    where
        T: Component + Interpolate + Clone + Debug,
    {
        self.add_systems(
            PreUpdate,
            (
                snapshot_buffer_init_system::<T>,
                snapshot_interpolation_system::<T>,
            )
                .after(ClientSet::Receive)
                .run_if(resource_exists::<RenetClient>()),
        );
        self.replicate_with::<T>(serialize, deserialize, remove)
    }
}
