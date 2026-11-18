use neolog_core::{Session, ViewId};

const ROS: &str = "\
[INFO] [1793440800.000000000] [mission_planner]: mission loaded: 12 waypoints
[INFO] [1793440800.250000000] [camera_driver]: camera opened /dev/video0 1920x1080
[WARN] [1793440800.500000000] [battery_monitor]: battery 22%, low
[INFO] [1793440801.000000000] [lidar_driver]: scan frame 1001, 2400 points
[ERROR] [1793440801.250000000] [lidar_driver]: timeout waiting for packet from 192.168.1.20
[WARN] [1793440801.400000000] [localization]: pose drift 0.42m exceeds threshold
[INFO] [1793440802.000000000] [controller]: cmd_vel x=0.32 w=0.10
[ERROR] [1793440802.500000000] [camera_driver]: frame dropped, buffer full
    at vision_pipeline.cpp:214
    at main.cpp:88
[INFO] [1793440803.000000000] [mission_planner]: waypoint 3 reached
[ERROR] [1793440803.400000000] [safety_monitor]: emergency stop: obstacle within 0.35m
[INFO] [1793440803.400000000] [controller]: 停止运动, 等待人工确认
[INFO] [1793440804.000000000] [lidar_driver]: packet stream recovered
";

const PYTHON: &str = "\
2026-10-31 10:00:00,500 - perception - INFO - model yolov8n loaded on cuda:0
2026-10-31 10:00:01,100 - perception - WARNING - frame 1001 skipped, queue full
2026-10-31 10:00:01,700 - tracker - ERROR - lost track id=42 after 8 frames
2026-10-31 10:00:02,300 - tracker - INFO - track id=42 reacquired
2026-10-31 10:00:02,900 - planner - CRITICAL - 路径规划失败：无可行解
Traceback (most recent call last):
  File \"planner.py\", line 112, in plan
    raise RuntimeError(\"no viable path\")
RuntimeError: no viable path
2026-10-31 10:00:03,600 - perception - DEBUG - inference 18.4ms
2026-10-31 10:00:04,200 - health - INFO - all sensors alive
";

pub fn load(session: &mut Session) -> Option<ViewId> {
    let ros = session.open_text("demo_robot_ros2.log", ROS);
    let py = session.open_text("demo_vision_python.log", PYTHON);
    session.create_doc_view(ros);
    session.create_doc_view(py);
    session.create_merged_view(&[ros, py], "合并 · 示例").ok()
}
