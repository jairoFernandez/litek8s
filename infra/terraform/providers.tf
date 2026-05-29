provider "aws" {
  region  = var.region
  profile = var.profile

  default_tags {
    tags = {
      Project   = "litek8s"
      ManagedBy = "terraform"
    }
  }
}
