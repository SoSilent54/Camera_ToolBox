#include <opencv2/calib.hpp>
#include <opencv2/core.hpp>
#include <opencv2/geometry.hpp>
#include <opencv2/imgcodecs.hpp>
#include <opencv2/objdetect.hpp>

#include <cmath>
#include <iostream>
#include <string>
#include <vector>

int main() {
  const std::string version = cv::getVersionString();
  if (version != "5.0.0") {
    std::cerr << "unexpected OpenCV version: " << version << '\n';
    return 1;
  }

  const cv::Mat gray(64, 64, CV_8UC1, cv::Scalar::all(0));
  std::vector<unsigned char> png;
  if (!cv::imencode(".png", gray, png) || png.empty()) {
    std::cerr << "PNG codec smoke failed\n";
    return 2;
  }

  std::vector<cv::Point2f> corners;
  const bool found = cv::findChessboardCorners(
      gray, cv::Size(3, 3), corners,
      cv::CALIB_CB_ADAPTIVE_THRESH | cv::CALIB_CB_NORMALIZE_IMAGE);
  if (found) {
    std::cerr << "blank image unexpectedly contains a chessboard\n";
    return 3;
  }

  const std::vector<cv::Point3f> object_points{{0.0F, 0.0F, 1.0F}};
  const cv::Mat rotation = cv::Mat::zeros(3, 1, CV_64F);
  const cv::Mat translation = cv::Mat::zeros(3, 1, CV_64F);
  const cv::Mat camera_matrix = cv::Mat::eye(3, 3, CV_64F);
  const cv::Mat distortion = cv::Mat::zeros(1, 5, CV_64F);
  std::vector<cv::Point2f> projected;
  cv::projectPoints(object_points, rotation, translation, camera_matrix,
                    distortion, projected);
  if (projected.size() != 1 || !std::isfinite(projected[0].x) ||
      !std::isfinite(projected[0].y)) {
    std::cerr << "point projection smoke failed\n";
    return 4;
  }

  bool rejected_empty_calibration = false;
  try {
    const std::vector<std::vector<cv::Point3f>> empty_object_points;
    const std::vector<std::vector<cv::Point2f>> empty_image_points;
    cv::Mat mutable_camera_matrix = cv::Mat::eye(3, 3, CV_64F);
    cv::Mat mutable_distortion = cv::Mat::zeros(1, 5, CV_64F);
    std::vector<cv::Mat> rotations;
    std::vector<cv::Mat> translations;
    static_cast<void>(cv::calibrateCamera(
        empty_object_points, empty_image_points, cv::Size(64, 64),
        mutable_camera_matrix, mutable_distortion, rotations, translations));
  } catch (const cv::Exception&) {
    rejected_empty_calibration = true;
  }
  if (!rejected_empty_calibration) {
    std::cerr << "empty calibration input was not rejected\n";
    return 5;
  }

  std::cout << "OpenCV " << version << " relocated CMake smoke passed\n";
  return 0;
}
