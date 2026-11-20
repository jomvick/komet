import Nav from "@/components/Nav";
import Hero from "@/components/Hero";
import Features from "@/components/Features";
import Downloads from "@/components/Downloads";
import AgentsOrbit from "@/components/AgentsOrbit";
import MemoryHandoff from "@/components/MemoryHandoff";
import AgentSwitch from "@/components/AgentSwitch";
import Topology from "@/components/Topology";
import UsageTable from "@/components/UsageTable";
import Faq from "@/components/Faq";
import Footer from "@/components/Footer";

export default function Home() {
  return (
    <>
      <Nav />
      <main>
        <Hero />
        <Features />
        <Downloads />
        <AgentsOrbit />
        <MemoryHandoff />
        <AgentSwitch />
        <Topology />
        <UsageTable />
        <Faq />
      </main>
      <Footer />
    </>
  );
}

