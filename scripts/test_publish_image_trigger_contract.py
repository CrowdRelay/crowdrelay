from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = (ROOT / ".github/workflows/publish-images.yml").read_text()


class PublishImageTriggerContract(unittest.TestCase):
    def test_successful_default_branch_ci_always_publishes(self) -> None:
        self.assertIn("workflow_run:", WORKFLOW)
        self.assertIn('workflows: ["CI"]', WORKFLOW)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", WORKFLOW)
        self.assertIn(
            "github.event.workflow_run.head_branch == github.event.repository.default_branch",
            WORKFLOW,
        )
        self.assertNotIn("github.event.workflow_run.event == 'push'", WORKFLOW)
        self.assertNotIn("github.event.workflow_run.event == 'workflow_dispatch'", WORKFLOW)

    def test_exact_validated_sha_drives_all_runtime_images(self) -> None:
        self.assertIn("IMAGE_SHA: ${{ github.event.workflow_run.head_sha }}", WORKFLOW)
        self.assertIn("crowdrelay-api:sha-${{ env.IMAGE_SHA }}", WORKFLOW)
        self.assertIn("crowdrelay-worker:sha-${{ env.IMAGE_SHA }}", WORKFLOW)
        self.assertIn("ref: ${{ env.IMAGE_SHA }}", WORKFLOW)


if __name__ == "__main__":
    unittest.main()
