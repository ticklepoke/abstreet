use core::future::Future;
use core::pin::Pin;

use anyhow::Result;
use rand_xorshift::XorShiftRng;

use abstio::MapName;
use abstutil::Timer;
use geom::Duration;
use map_model::{EditCmd, EditIntersectionControl, MapEdits};
use sim::ScenarioGenerator;
use synthpop::{OrigPersonID, Scenario, ScenarioModifier};
use widgetry::{
    lctrl, EventCtx, GeomBatch, GfxCtx, Key, Line, Outcome, Panel, State, TextExt, Widget,
};

pub use self::freeform::spawn_agents_around;
pub use self::tutorial::{Tutorial, TutorialPointer, TutorialState};
use crate::app::App;
use crate::app::Transition;
use crate::challenges::{Challenge, ChallengesPicker};
use crate::edit::SaveEdits;
use crate::pregame::TitleScreen;
use crate::sandbox::{Actions, SandboxControls, SandboxMode};

// TODO pub so challenges can grab cutscenes and SandboxMode can dispatch to actions. Weird?
mod actdev;
pub mod commute;
pub mod fix_traffic_signals;
pub mod freeform;
pub mod play_scenario;
pub mod tutorial;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum GameplayMode {
    // TODO Maybe this should be "sandbox"
    Freeform(MapName),
    // Map name, scenario name
    PlayScenario(MapName, String, Vec<ScenarioModifier>),
    FixTrafficSignals,
    OptimizeCommute(OrigPersonID, Duration),
    // Map name, scenario name, background traffic
    Actdev(MapName, String, bool),

    // current
    Tutorial(TutorialPointer),
}

pub trait GameplayState: downcast_rs::Downcast {
    fn event(
        &mut self,
        ctx: &mut EventCtx,
        app: &mut App,
        controls: &mut SandboxControls,
        actions: &mut Actions,
    ) -> Option<Transition>;
    fn draw(&self, g: &mut GfxCtx, app: &App);
    fn on_destroy(&self, _: &mut App) {}
    fn recreate_panels(&mut self, ctx: &mut EventCtx, app: &App);

    fn can_move_canvas(&self) -> bool {
        true
    }
    fn can_examine_objects(&self) -> bool {
        true
    }
    fn has_common(&self) -> bool {
        true
    }
    fn has_tool_panel(&self) -> bool {
        true
    }
    fn has_time_panel(&self) -> bool {
        true
    }
    fn has_minimap(&self) -> bool {
        true
    }
}
downcast_rs::impl_downcast!(GameplayState);

pub enum LoadScenario {
    Nothing,
    Path(String),
    Scenario(Scenario),
    // wasm futures are not `Send`, since they all ultimately run on the browser's single threaded
    // runloop
    #[cfg(target_arch = "wasm32")]
    Future(Pin<Box<dyn Future<Output = Result<Box<dyn Send + FnOnce(&App) -> Scenario>>>>>),
    #[cfg(not(target_arch = "wasm32"))]
    Future(Pin<Box<dyn Send + Future<Output = Result<Box<dyn Send + FnOnce(&App) -> Scenario>>>>>),
}

impl GameplayMode {
    pub fn map_name(&self) -> MapName {
        match self {
            GameplayMode::Freeform(ref name) => name.clone(),
            GameplayMode::PlayScenario(ref name, _, _) => name.clone(),
            GameplayMode::FixTrafficSignals => MapName::seattle("downtown"),
            GameplayMode::OptimizeCommute(_, _) => MapName::seattle("montlake"),
            GameplayMode::Tutorial(_) => MapName::seattle("montlake"),
            GameplayMode::Actdev(ref name, _, _) => name.clone(),
        }
    }

    pub fn scenario(&self, app: &App, mut rng: XorShiftRng, timer: &mut Timer) -> LoadScenario {
        let map = &app.primary.map;
        let name = match self {
            GameplayMode::Freeform(_) => {
                let mut s = Scenario::empty(map, "empty");
                s.only_seed_buses = None;
                return LoadScenario::Scenario(s);
            }
            GameplayMode::PlayScenario(_, ref scenario, _) => scenario.to_string(),
            GameplayMode::Tutorial(current) => {
                return match Tutorial::scenario(app, *current) {
                    Some(generator) => {
                        LoadScenario::Scenario(generator.generate(map, &mut rng, timer))
                    }
                    None => LoadScenario::Nothing,
                };
            }
            GameplayMode::Actdev(_, ref scenario, bg_traffic) => {
                if *bg_traffic {
                    format!("{}_with_bg", scenario)
                } else {
                    scenario.to_string()
                }
            }
            GameplayMode::FixTrafficSignals | GameplayMode::OptimizeCommute(_, _) => {
                "weekday".to_string()
            }
        };
        if name == "random" {
            LoadScenario::Scenario(ScenarioGenerator::small_run(map).generate(map, &mut rng, timer))
        } else if name == "home_to_work" {
            LoadScenario::Scenario(ScenarioGenerator::proletariat_robot(map, &mut rng, timer))
        } else if name == "census" {
            let map_area = map.get_boundary_polygon().clone();
            let map_bounds = map.get_gps_bounds().clone();
            let mut rng = sim::fork_rng(&mut rng);

            LoadScenario::Future(Box::pin(async move {
                let areas = popdat::CensusArea::fetch_all_for_map(&map_area, &map_bounds).await?;

                let scenario_from_app: Box<dyn Send + FnOnce(&App) -> Scenario> =
                    Box::new(move |app: &App| {
                        let config = popdat::Config::default();
                        popdat::generate_scenario(
                            "typical monday",
                            areas,
                            config,
                            &app.primary.map,
                            &mut rng,
                        )
                    });

                Ok(scenario_from_app)
            }))
        } else {
            LoadScenario::Path(abstio::path_scenario(map.get_name(), &name))
        }
    }

    pub fn can_edit_roads(&self) -> bool {
        !matches!(self, GameplayMode::FixTrafficSignals)
    }

    pub fn can_edit_stop_signs(&self) -> bool {
        !matches!(self, GameplayMode::FixTrafficSignals)
    }

    pub fn can_jump_to_time(&self) -> bool {
        !matches!(self, GameplayMode::Freeform(_))
    }

    pub fn allows(&self, edits: &MapEdits) -> bool {
        for cmd in &edits.commands {
            match cmd {
                EditCmd::ChangeRoad { .. } => {
                    if !self.can_edit_roads() {
                        return false;
                    }
                }
                EditCmd::ChangeIntersection {
                    ref new, ref old, ..
                } => {
                    match new.control {
                        // TODO Conflating construction
                        EditIntersectionControl::StopSign(_) | EditIntersectionControl::Closed => {
                            if !self.can_edit_stop_signs() {
                                return false;
                            }
                        }
                        _ => {}
                    }
                    // TODO Another hack to see if we can only edit signal timing
                    if old.crosswalks != new.crosswalks && !self.can_edit_stop_signs() {
                        return false;
                    }
                }
                EditCmd::ChangeRouteSchedule { .. } => {}
                EditCmd::ChangeBuilding { .. } => {}
            }
        }
        true
    }

    /// Must be called after the scenario has been setup. The caller will call recreate_panels
    /// after this, so each constructor doesn't need to.
    pub fn initialize(&self, ctx: &mut EventCtx, app: &mut App) -> Box<dyn GameplayState> {
        match self {
            GameplayMode::Freeform(_) => freeform::Freeform::new_state(ctx, app),
            GameplayMode::PlayScenario(_, ref scenario, ref modifiers) => {
                play_scenario::PlayScenario::new_state(ctx, app, scenario, modifiers.clone())
            }
            GameplayMode::FixTrafficSignals => {
                fix_traffic_signals::FixTrafficSignals::new_state(ctx)
            }
            GameplayMode::OptimizeCommute(p, goal) => {
                commute::OptimizeCommute::new_state(ctx, app, *p, *goal)
            }
            GameplayMode::Tutorial(current) => Tutorial::make_gameplay(ctx, app, *current),
            GameplayMode::Actdev(_, ref scenario, bg_traffic) => {
                actdev::Actdev::new_state(ctx, scenario.clone(), *bg_traffic)
            }
        }
    }
}

fn challenge_header(ctx: &mut EventCtx, title: &str) -> Widget {
    Widget::row(vec![
        Line(title).small_heading().into_widget(ctx).centered_vert(),
        ctx.style()
            .btn_plain
            .icon("system/assets/tools/info.svg")
            .build_widget(ctx, "instructions")
            .centered_vert(),
        Widget::vert_separator(ctx, 50.0),
        ctx.style()
            .btn_outline
            .icon_text("system/assets/tools/pencil.svg", "Edit map")
            .hotkey(lctrl(Key::E))
            .build_widget(ctx, "edit map")
            .centered_vert(),
    ])
    .padding(5)
}

pub struct FinalScore {
    panel: Panel,
    retry: GameplayMode,
    next_mode: Option<GameplayMode>,

    chose_next: bool,
    chose_back_to_challenges: bool,
}

impl FinalScore {
    pub fn new_state(
        ctx: &mut EventCtx,
        msg: String,
        mode: GameplayMode,
        next_mode: Option<GameplayMode>,
    ) -> Box<dyn State<App>> {
        Box::new(FinalScore {
            panel: Panel::new_builder(Widget::row(vec![
                GeomBatch::load_svg(ctx, "system/assets/characters/boss.svg.gz")
                    .scale(0.75)
                    .autocrop()
                    .into_widget(ctx)
                    .container()
                    .section(ctx),
                Widget::col(vec![
                    msg.text_widget(ctx),
                    // TODO Adjust wording
                    ctx.style()
                        .btn_outline
                        .text("Keep simulating")
                        .build_def(ctx),
                    ctx.style().btn_outline.text("Try again").build_def(ctx),
                    if next_mode.is_some() {
                        ctx.style()
                            .btn_solid_primary
                            .text("Next challenge")
                            .build_def(ctx)
                    } else {
                        Widget::nothing()
                    },
                    ctx.style()
                        .btn_outline
                        .text("Back to challenges")
                        .build_def(ctx),
                ])
                .section(ctx),
            ]))
            .build(ctx),
            retry: mode,
            next_mode,
            chose_next: false,
            chose_back_to_challenges: false,
        })
    }
}

impl State<App> for FinalScore {
    fn event(&mut self, ctx: &mut EventCtx, app: &mut App) -> Transition {
        if let Outcome::Clicked(x) = self.panel.event(ctx) {
            match x.as_ref() {
                "Keep simulating" => {
                    return Transition::Pop;
                }
                "Try again" => {
                    return Transition::Multi(vec![
                        Transition::Pop,
                        Transition::Replace(SandboxMode::simple_new(app, self.retry.clone())),
                    ]);
                }
                "Next challenge" => {
                    self.chose_next = true;
                    if app.primary.map.unsaved_edits() {
                        return Transition::Push(SaveEdits::new_state(
                            ctx,
                            app,
                            "Do you want to save your proposal first?",
                            true,
                            None,
                            Box::new(|_, _| {}),
                        ));
                    }
                }
                "Back to challenges" => {
                    self.chose_back_to_challenges = true;
                    if app.primary.map.unsaved_edits() {
                        return Transition::Push(SaveEdits::new_state(
                            ctx,
                            app,
                            "Do you want to save your proposal first?",
                            true,
                            None,
                            Box::new(|_, _| {}),
                        ));
                    }
                }
                _ => unreachable!(),
            }
        }

        if self.chose_next || self.chose_back_to_challenges {
            app.clear_everything(ctx);
        }

        if self.chose_next {
            return Transition::Clear(vec![
                TitleScreen::new_state(ctx, app),
                // Constructing the cutscene doesn't require the map/scenario to be loaded.
                SandboxMode::simple_new(app, self.next_mode.clone().unwrap()),
                (Challenge::find(self.next_mode.as_ref().unwrap())
                    .0
                    .cutscene
                    .unwrap())(ctx, app, self.next_mode.as_ref().unwrap()),
            ]);
        }
        if self.chose_back_to_challenges {
            return Transition::Clear(vec![
                TitleScreen::new_state(ctx, app),
                ChallengesPicker::new_state(ctx, app),
            ]);
        }

        Transition::Keep
    }

    fn draw(&self, g: &mut GfxCtx, _: &App) {
        self.panel.draw(g);
    }
}
