// src/simulation/systems.rs

use crate::integrators::RK4;
use crate::path_planning::grid_utils::{GridConfig, ObstacleGrid};
// Or your preferred default integrator
use crate::simulation::{
    components::*, // Import all components
    traits::*,     // Import all traits
    utils::*,      // Import all utility functions
};
use bevy::prelude::*;
use nalgebra::{Isometry3, Point3, UnitQuaternion, Vector3}; // For path following, obstacle collection
use std::collections::HashMap; // For estimator event batching

// Define constants for simulation rates (Hz)
// These might be configured elsewhere later (e.g., Resources or config files)
pub const PLANNER_HZ: f64 = 10.0;
pub const CONTROLLER_HZ: f64 = 30.0;
pub const ESTIMATOR_HZ: f64 = 50.0;
pub const DYNAMICS_HZ: f64 = 100.0; // Simulation physics loop rate

// =========================================================================
// == Path Planning System ==
// =========================================================================

/// System responsible for running path planners for autonomous agents.
pub fn planner_system(
    mut commands: Commands,
    grid_config: Res<GridConfig>,
    obstacle_grid: Res<ObstacleGrid>,
    // Query for world obstacles to pass to the planner trait
    obstacle_query: Query<(&GlobalTransform, &ObstacleComponent)>, // Use GlobalTransform for world pose
    mut agent_query: Query<
        (
            Entity,
            &mut CurrentPath,
            &PathPlannerLogic,
            &GoalComponent,
            &TrueState,
            Option<&DynamicsModel>,
        ),
        (
            With<AutonomousAgent>,
            Or<(Added<GoalComponent>, Added<CurrentPath>)>,
        ),
    >,
    time: Res<Time>,
) {
    if obstacle_grid.width == 0 || obstacle_grid.height == 0 {
        return;
    }

    // --- Collect Obstacles ONCE per system run ---
    // This is still potentially inefficient if many agents plan, but better
    // than querying inside the loop. Consider passing query directly if feasible.
    let obstacles_vec: Vec<Obstacle> = obstacle_query
        .iter()
        .map(|(transform, obs_comp)| {
            // Create the Obstacle struct matching the trait definition
            Obstacle {
                id: obs_comp.0.id, // Assuming ObstacleComponent holds an Obstacle struct
                pose: bevy_global_transform_to_nalgebra_isometry(transform), // Convert Bevy transform
                shape: obs_comp.0.shape.clone(),
                is_static: obs_comp.0.is_static,
            }
        })
        .collect();
    // --- End Obstacle Collection ---

    for (entity, mut current_path, planner_logic, goal, true_state, dynamics_model_opt) in
        agent_query.iter_mut()
    {
        if !current_path.0.is_empty() {
            continue;
        }
        println!("Entity {:?}: Planning path...", entity);

        let planner = &planner_logic.0;
        let dynamics_opt = dynamics_model_opt.map(|dm| dm.0.as_ref());

        // Call plan_path with all arguments required by the trait
        let maybe_path_result = planner.plan_path(
            // Renamed to clarify it's a Result
            &true_state.0,
            &goal.0,
            &obstacles_vec, // <-- Pass the collected obstacles slice
            dynamics_opt,
            &grid_config,
            &obstacle_grid, // Pass the grid resource directly (it's already a ref)
            time.elapsed().as_secs_f64(),
        );

        // Match the Result
        match maybe_path_result {
            Ok(new_path) => {
                if !new_path.is_empty() {
                    println!(
                        "Entity {:?}: Path found ({} waypoints).",
                        entity,
                        new_path.len()
                    );
                    current_path.0 = new_path;
                } else {
                    // Planner succeeded but returned an empty path (might be valid if start=goal)
                    println!("Entity {:?}: Planner returned empty path.", entity);
                    current_path.0.clear();
                }
            }
            Err(e) => {
                // Handle planning errors
                println!("Entity {:?}: Path planning failed: {}", entity, e); // Use Display impl of error
                current_path.0.clear(); // Clear path on failure
            }
        }
    }
}

/// System for AUTONOMOUS agents to calculate control inputs.
/// (Could be merged with keyboard control if desired, but separation is clear)
pub fn autonomous_controller_system(
    mut agent_query: Query<
        (
            Entity,
            &mut ControlInput,
            &mut ControllerLogic,
            &GoalComponent, // Keep goal for context, even if follower uses path
            &TrueState,     // Ideal: EstimatedState
            Option<&mut CurrentPath>, // Path is primary input for follower
            Option<&DynamicsModel>,
        ),
        With<AutonomousAgent>,
    >,
    time: Res<Time>,
) {
    for (
        _entity,
        mut control_input,
        mut controller_logic,
        goal_component, // Use this goal if path is empty?
        true_state,
        mut path_opt, // Note: this is Option<&mut CurrentPath>
        dynamics_model_opt,
    ) in agent_query.iter_mut()
    {
        let controller = &mut controller_logic.0;
        let dynamics_opt: Option<&dyn Dynamics> = dynamics_model_opt.map(|dm| dm.0.as_ref());

        // Pass path_opt directly to the controller
        let u = controller.calculate_control(
            &true_state.0,
            &goal_component.0,
            dynamics_opt,
            path_opt.as_deref_mut(), // Pass the Option<&mut CurrentPath>
            time.elapsed().as_secs_f64(),
        );

        control_input.0 = u;
    }
}

// =========================================================================
// == Dynamics System ==
// =========================================================================

/// System responsible for updating the TrueState of entities based on dynamics and control inputs.
/// Runs at a fixed, high frequency.
pub fn dynamics_system(
    mut query: Query<
        (Entity, &mut TrueState, &DynamicsModel, &ControlInput),
        Without<ObstacleComponent>,
    >, // Don't move obstacles unless they also have Dynamics
    time: Res<Time>, // Use Bevy's Time resource
    // Store the chosen integrator locally or make it a Resource
    // Using RK4 as an example. Ensure your integrators are Send + Sync if used across threads.
    integrator: Local<RK4>,
    // Access simulation step dt directly if using FixedUpdate
    // fixed_time: Res<FixedTime>,
) {
    // Get the time step for the fixed update.
    // Using time.delta_seconds() works if the system *only* runs within FixedUpdate.
    // If it could run elsewhere, explicitly use the FixedTime resource.
    let dt = time.delta().as_secs_f64();
    // let dt = fixed_time.period.as_secs_f64(); // Alternative if FixedTime resource is available

    if dt <= 0.0 {
        return; // Avoid division by zero or weirdness if dt is invalid
    }

    // Use parallel iteration if many dynamic entities exist
    query
        .par_iter_mut()
        .for_each(|(_entity, mut true_state, dynamics_model, control_input)| {
            let current_state = &true_state.0;
            let current_control = &control_input.0; // Read the latest control input

            // Get the dynamics trait object
            let dynamics = &dynamics_model.0;

            // Propagate the state forward using the integrator
            let next_state = dynamics.propagate(
                current_state,
                current_control,
                time.elapsed().as_secs_f64(), // Current time T0 for propagation
                dt,                           // Time step dt
                &*integrator,                 // Pass the chosen integrator
            );

            // Update the true state component
            true_state.0 = next_state;

            // TODO: If using Bevy Transform directly, update it here based on the relevant
            // parts of the new true_state.0 (e.g., position, orientation). Requires mapping
            // state vector indices to Transform components.
            // Example (assuming state = [x, y, z, qx, qy, qz, qw, ...]):
            // transform.translation = Vec3::new(next_state[0], next_state[1], next_state[2]);
            // transform.rotation = Quat::from_xyzw(next_state[3], next_state[4], next_state[5], next_state[6]);
        });
}

// =========================================================================
// == Sensor Simulation System ==
// =========================================================================

/// System that simulates sensor readings based on TrueState and environment obstacles.
/// Fires SensorOutputEvents. Runs frequently (e.g., in Update) to check sensor timers.
pub fn sensor_system(
    mut sensor_query: Query<(Entity, &GlobalTransform, &mut SensorSuite, &TrueState)>,
    obstacle_query: Query<(&GlobalTransform, &ObstacleComponent)>,
    time: Res<Time>,
    mut sensor_event_writer: EventWriter<SensorOutputEvent>,
) {
    let delta_time = time.delta(); // Bevy's delta Duration
    let current_sim_time = time.elapsed().as_secs_f64();

    // --- Obstacle Collection (Similar to planner_system) ---
    // TODO: Optimization needed here for large numbers of obstacles.
    let obstacles: Vec<Obstacle> = obstacle_query
        .iter()
        .map(|(transform, obs_comp)| Obstacle {
            id: obs_comp.0.id,
            pose: bevy_global_transform_to_nalgebra_isometry(transform),
            shape: obs_comp.0.shape.clone(),
            is_static: obs_comp.0.is_static,
        })
        .collect();

    // --- Sensor Simulation Loop ---
    for (entity, agent_transform, mut sensor_suite, true_state) in sensor_query.iter_mut() {
        let agent_world_pose = agent_transform.compute_transform();

        for sensor_instance in sensor_suite.sensors.iter_mut() {
            // Tick the sensor's individual timer
            sensor_instance.timer.tick(delta_time);

            // Check if sensor should run based on its rate
            let should_run =
                sensor_instance.update_rate_hz.is_none() || sensor_instance.timer.finished();

            if should_run {
                // Revised sensor_system pose calculation:
                let sensor_offset_transform = Transform {
                    translation: Vec3::new(
                        sensor_instance.pose_offset.translation.x as f32,
                        sensor_instance.pose_offset.translation.y as f32,
                        sensor_instance.pose_offset.translation.z as f32,
                    ),
                    rotation: Quat::from_xyzw(
                        // Bevy Quat is x,y,z,w
                        sensor_instance.pose_offset.rotation.i as f32,
                        sensor_instance.pose_offset.rotation.j as f32,
                        sensor_instance.pose_offset.rotation.k as f32,
                        sensor_instance.pose_offset.rotation.w as f32,
                    ),
                    ..Default::default() // Scale = 1
                };
                // Calculate the world pose of the sensor
                let sensor_world_bevy_transform = agent_world_pose * sensor_offset_transform;
                let sensor_world_pose_nalgebra: Isometry3<f64> =
                    bevy_transform_to_nalgebra_isometry(&sensor_world_bevy_transform);

                // Get the sensor model trait object
                let sensor_model = &sensor_instance.model;

                // Call the sense method
                let sensor_output = sensor_model.sense(
                    &true_state.0,               // Provide robot's true state
                    &sensor_world_pose_nalgebra, // Provide sensor's true world pose
                    &obstacles,                  // Provide environment obstacles
                    current_sim_time,            // Provide current time
                );

                // Send event if data was generated
                match sensor_output {
                    SensorOutputData::Empty { .. } => { /* Do nothing */ }
                    data => {
                        sensor_event_writer.write(SensorOutputEvent {
                            entity, // Associate event with the entity owning the sensor
                            data,   // The actual sensor data
                        });
                    }
                }
            }
        }
    }
}

// =========================================================================
// == State Estimation System ==
// =========================================================================

// --- Separate Predict/Update methods in Estimator Trait are Recommended ---
// Let's assume we modified the Estimator trait as discussed:
// pub trait Estimator: Debug + Send + Sync {
//     fn predict(&mut self, control: &Control, dynamics: &dyn Dynamics, dt: Time);
//     fn update(&mut self, measurement: &SensorOutputData);
//     fn get_current_estimate(&self) -> State;
//     fn get_covariance(&self) -> Option<DMatrix<f64>>;
// }
// If still using the single `estimate` function, the logic below needs adaptation.

/// System responsible for running the prediction step of state estimators.
/// Runs at a fixed rate (e.g., ESTIMATOR_HZ).
pub fn estimator_predict_system(
    mut estimator_query: Query<
        (Entity, &mut EstimatorLogic, &ControlInput, &DynamicsModel),
        With<AutonomousAgent>,
    >,
    time: Res<Time>,
) {
    let dt = time.delta().as_secs_f64();
    if dt <= 0.0 {
        return;
    }

    for (_entity, mut estimator_logic, control_input, dynamics_model) in estimator_query.iter_mut()
    {
        let estimator = &mut estimator_logic.0;
        let dynamics = dynamics_model.0.as_ref(); // Get &dyn Dynamics
        let control = &control_input.0;

        estimator.predict(control, dynamics, dt);
    }
}

/// System responsible for running the update step of state estimators based on sensor events.
/// Should run after `estimator_predict_system`.
pub fn estimator_update_system(
    mut estimator_query: Query<(Entity, &mut EstimatorLogic), With<AutonomousAgent>>,
    mut sensor_events: EventReader<SensorOutputEvent>,
) {
    // Batch events by entity for efficiency
    let mut events_by_entity: HashMap<Entity, Vec<SensorOutputData>> = HashMap::new();
    for event in sensor_events.read() {
        events_by_entity
            .entry(event.entity)
            .or_default()
            .push(event.data.clone());
    }

    if events_by_entity.is_empty() {
        return; // No sensor events to process
    }

    // Process updates for entities that received sensor data
    for (entity, mut estimator_logic) in estimator_query.iter_mut() {
        if let Some(measurements) = events_by_entity.get(&entity) {
            let estimator = &mut estimator_logic.0;
            for measurement in measurements {
                estimator.update(measurement);
            }
        }
    }
}

/// System responsible for writing the final estimate from EstimatorLogic to the EstimatedState component.
/// Should run after `estimator_update_system`.
pub fn estimator_write_output_system(
    mut query: Query<
        (&mut EstimatedState, &EstimatorLogic),
        (With<AutonomousAgent>, Changed<EstimatorLogic>),
    >, // Only run if EstimatorLogic changed
) {
    for (mut estimated_state_comp, estimator_logic) in query.iter_mut() {
        let estimator = &estimator_logic.0;
        estimated_state_comp.state = estimator.get_current_estimate();
        estimated_state_comp.covariance = estimator.get_covariance();
    }
}

// =========================================================================
// == System Setup Function (Example - add to main.rs) ==
// =========================================================================
/*
// In main.rs or a plugin setup function:

use bevy::core::FixedTimestep; // Older FixedUpdate timestep system
// Or use newer rate limiting conditions if preferred

fn setup_simulation_systems(app: &mut App) {
    app
        // Add the SensorOutputEvent
        .add_event::<SensorOutputEvent>()

        // Configure FixedUpdate simulation loop stages
        // Define stages for clear execution order if needed
        .add_stage_before(
            CoreStage::Update,
            "SimulationFixedUpdate",
            SystemStage::parallel()
                .with_run_criteria(FixedTimestep::step(1.0 / DYNAMICS_HZ)) // Highest frequency base step
        )
        // Could add more stages for different rates or use simple .with_system_set

        // --- Schedule Systems ---

        // Dynamics runs at the highest rate
        .add_system_to_stage(
             "SimulationFixedUpdate",
             dynamics_system
        )

        // Controller runs less frequently
        .add_system_set_to_stage(
            "SimulationFixedUpdate",
            SystemSet::new()
                .with_run_criteria(FixedTimestep::step(1.0 / CONTROLLER_HZ))
                .with_system(controller_system),
        )

        // Planner runs least frequently
        .add_system_set_to_stage(
            "SimulationFixedUpdate",
            SystemSet::new()
                .with_run_criteria(FixedTimestep::step(1.0 / PLANNER_HZ))
                .with_system(planner_system),
        )

        // Estimator prediction runs at its own rate
        .add_system_set_to_stage(
            "SimulationFixedUpdate",
            SystemSet::new()
                .with_run_criteria(FixedTimestep::step(1.0 / ESTIMATOR_HZ))
                .with_system(estimator_predict_system),
                // Ordering: Ensure prediction runs before update/output if they are in the same stage
                // .with_system(estimator_update_system.after(estimator_predict_system))
                // .with_system(estimator_write_output_system.after(estimator_update_system)),
         )
         // Estimator update/output need to run *after* prediction for the current step
         // Could run in a stage *after* SimulationFixedUpdate or carefully ordered within it.
         // Running them here ensures they consider predictions from the *same* step.
         .add_system_set_to_stage(
            CoreStage::PostUpdate, // Run after main updates & potentially after fixed updates process
            SystemSet::new()
                 .with_system(estimator_update_system) // Process events collected during the frame
                 .with_system(estimator_write_output_system.after(estimator_update_system)) // Write final state
         )


        // Sensor system runs in the main Update loop to check timers every frame
        .add_system_to_stage(CoreStage::Update, sensor_system);

}
*/
